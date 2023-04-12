//! # v2
//!
//! This is a reimplementation of the stupid things in Arctic

use async_trait::async_trait;
use btleplug::{
    api::{Central, Manager as _, Peripheral as _, ScanFilter},
    platform::{Manager, Peripheral},
};
use futures::StreamExt;
use tracing::instrument;
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::{
    polar_uuid::NotifyUuid, ControlPoint, ControlPointCommand, ControlResponse, Error,
    H10MeasurementType, HeartRate, NotifyStream, PmdRead, PolarResult, SupportedFeatures,
};

/// Trait for handling events coming from a device
#[async_trait]
pub trait EventHandler {
    /// Dispatched when a battery update is received.
    ///
    /// Contains the current battery level.
    async fn battery_update(&self, _battery_level: u8) {}

    /// Dispatched when a heart rate update is received.
    ///
    /// Contains information about the heart rate and R-R timing.
    async fn heart_rate_update(&self, _heartrate: HeartRate) {}

    /// Dispatched when measurement data is received over the PMD data UUID.
    ///
    /// Contains data in a [`PmdRead`].
    async fn measurement_update(&self, _data: PmdRead) {}
}

/// The core sensor manager
pub struct PolarSensor<L: Level> {
    ble_manager: Manager,
    ble_device: Option<Peripheral>,
    control_point: Option<ControlPoint>,
    data_types: Vec<EventType>,
    range: u8,
    sample_rate: u8,
    level: L,
}

/// Types the [`PolarSensor`] can listen for
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    /// Heart rate: RR rate and BPM
    Hr,
    /// Acceleration: mG
    Acc,
    /// Electrocardiagram: V
    Ecg,
    /// Batter
    Battery,
}

impl EventType {
    /// Convert [`EventType`] into its [`Uuid[]`]
    pub fn to_uuid(&self) -> Uuid {
        NotifyUuid::from(*self).into()
    }

    /// Try to convert [`EventType`] to an [`H10MeasurementType`]
    pub fn as_h10(&self) -> Option<H10MeasurementType> {
        match self {
            Self::Ecg => Some(H10MeasurementType::Ecg),
            Self::Acc => Some(H10MeasurementType::Acc),
            _ => None,
        }
    }
}

impl PolarSensor<Bluetooth> {
    /// Construct a PolarSensor
    pub async fn new() -> PolarResult<Self> {
        Ok(Self {
            ble_manager: Manager::new().await?,
            ble_device: None,
            control_point: None,
            data_types: vec![],
            range: 8,
            sample_rate: 200,
            level: Bluetooth,
        })
    }

    /// Connect to a device. Blocks until a connection is found
    #[instrument(skip(self))]
    pub async fn block_connect(mut self, device_id: &str) -> PolarResult<PolarSensor<Configure>> {
        while !self.is_connected().await {
            match self.try_connect(device_id).await {
                Err(e @ Error::NoBleAdaptor) => {
                    tracing::error!("no bluetooth adaptors found");
                    return Err(e);
                }
                Err(e) => tracing::warn!("could not connect: {}", e),
                Ok(_) => {}
            }
        }
        let new_self: PolarSensor<Configure> = PolarSensor {
            ble_manager: self.ble_manager,
            ble_device: self.ble_device,
            control_point: self.control_point,
            data_types: self.data_types,
            range: self.range,
            sample_rate: self.sample_rate,
            level: Configure::default(),
        };

        Ok(new_self)
    }

    /// Connect to a device, but override the behavior after each attempted connect
    /// Return [`Ok`] from the closure to continue trying to connect or [`Err`]
    /// give up and return.
    ///
    /// ## Examples
    ///
    /// ```rust,no_run
    /// # #[tokio::main]
    /// # async fn main() {
    /// use arctic::{Error, v2::PolarSensor};
    ///
    /// let mut polar = PolarSensor::new().await.unwrap()
    ///     // default handling that is applied to PolarSensor::block_connect
    ///     .map_connect("7B45F72B", |r| {
    ///         match r {
    ///             Err(e @ Error::NoBleAdaptor) => {
    ///                 tracing::error!("no bluetooth adaptors found");
    ///                 return Err(e);
    ///             }
    ///             Err(e) => tracing::warn!("could not connect: {}", e),
    ///             Ok(_) => {}
    ///         };
    ///         Ok(())
    ///     }).await.unwrap();
    /// # }
    /// ```
    #[instrument(skip(self, f))]
    pub async fn map_connect<F>(
        mut self,
        device_id: &str,
        mut f: F,
    ) -> PolarResult<PolarSensor<Configure>>
    where
        F: FnMut(PolarResult<()>) -> PolarResult<()>,
    {
        while !self.is_connected().await {
            if let Err(e) = f(self.try_connect(device_id).await) {
                return Err(e);
            }
        }
        let new_self: PolarSensor<Configure> = PolarSensor {
            ble_manager: self.ble_manager,
            ble_device: self.ble_device,
            control_point: self.control_point,
            data_types: self.data_types,
            range: self.range,
            sample_rate: self.sample_rate,
            level: Configure::default(),
        };

        Ok(new_self)
    }

    async fn is_connected(&self) -> bool {
        // async in iterators when?
        // self.ble_device
        //     .and_then(|d| d.is_connected().await.ok())
        //     .ok_or(false)
        if let Some(device) = &self.ble_device {
            if let Ok(v) = device.is_connected().await {
                return v;
            }
        }
        false
    }

    /// Try to connect to a device. Implements the v1 [`crate::PolarSensor::connect`] function
    #[instrument(skip(self))]
    async fn try_connect(&mut self, device_id: &str) -> PolarResult<()> {
        tracing::debug!("trying to connect");
        let adapters = self
            .ble_manager
            .adapters()
            .await
            .map_err(|_| Error::NoBleAdaptor)?;
        let Some(central) = adapters.first() else {
            tracing::error!("no ble adaptor found");
            return Err(Error::NoBleAdaptor);
        };

        central.start_scan(ScanFilter::default()).await?;
        tokio::time::sleep(Duration::from_secs(2)).await;

        for p in central.peripherals().await? {
            if p.properties()
                .await?
                .unwrap()
                .local_name
                .iter()
                .any(|name| name.starts_with("Polar") && name.ends_with(device_id))
            {
                self.ble_device = Some(p);
                break;
            }
        }

        let Some(device) = &self.ble_device else {
            tracing::warn!("device not found");
            return Err(Error::NoDevice);
        };

        device.connect().await?;
        device.discover_services().await?;

        let controller = ControlPoint::new(device).await?;
        self.control_point = Some(controller);

        Ok(())
    }
}

impl PolarSensor<Configure> {
    /// Add a data type to listen to
    #[instrument(skip(self))]
    pub fn listen(mut self, ty: EventType) -> PolarResult<Self> {
        if self.data_types.contains(&ty) {
            return Ok(self);
        }
        tracing::info!("{ty:?} added to data_types");

        match ty {
            EventType::Hr => {
                if !self.level.heart_rate {
                    self.level.heart_rate = true;
                }
            }
            EventType::Acc | EventType::Ecg => {
                if !self.level.measurement_data {
                    self.level.measurement_data = true;
                }
            }
            EventType::Battery => {
                if !self.level.battery {
                    self.level.battery = true;
                }
            }
        }

        self.data_types.push(ty);

        Ok(self)
    }

    /// Set data range for acceleration data: 2, 4 or 8
    #[instrument(skip(self))]
    pub fn range(mut self, range: u8) -> Self {
        tracing::debug!("setting range to {range}");
        if range == 2 || range == 4 || range == 8 {
            self.range = range;
        }

        self
    }

    /// Set sample rate for acceleration data
    #[instrument(skip(self))]
    pub fn sample_rate(mut self, rate: u8) -> Self {
        tracing::debug!("setting sample rate to {rate}");
        if rate == 25 || rate == 50 || rate == 100 || rate == 200 {
            self.sample_rate = rate;
        }

        self
    }

    /// Produce the sensor ready for build
    #[instrument(skip(self))]
    pub async fn build(self) -> PolarResult<PolarSensor<EventLoop>> {
        if self.level.heart_rate {
            self.subscribe(EventType::Hr.into()).await?;
        }
        if self.level.measurement_data {
            // [`EventType::Acc`] and [`EventType::Ecg`] will have
            // the same effect in this function
            self.subscribe(EventType::Acc.into()).await?;
        }
        if self.level.battery {
            self.subscribe(EventType::Battery.into()).await?;
        }

        // make sure that we aren't listening for data we haven't requested
        // in the event loop we'll request the measurement start for everything
        // we need
        tracing::info!("ensuring measurements from previous connections are stopped");
        self.get_pmd_response(
            ControlPointCommand::StopMeasurement,
            H10MeasurementType::Acc,
        )
        .await?;
        self.get_pmd_response(
            ControlPointCommand::StopMeasurement,
            H10MeasurementType::Ecg,
        )
        .await?;

        Ok(PolarSensor {
            level: EventLoop,
            ble_manager: self.ble_manager,
            ble_device: self.ble_device,
            control_point: self.control_point,
            data_types: self.data_types,
            range: self.range,
            sample_rate: self.sample_rate,
        })
    }
}

impl PolarSensor<EventLoop> {
    /// Start the event loop
    #[instrument(skip_all)]
    pub async fn event_loop<H: EventHandler + Sync + Send + 'static>(
        self,
        handler: H,
    ) -> PolarHandle {
        tracing::info!("starting measurements");
        // subscribe to the data types we want
        for ty in &self.data_types {
            use EventType::*;
            if let ty @ (Ecg | Acc) = ty {
                let _ = self
                    .get_pmd_response(
                        ControlPointCommand::RequestMeasurementStart,
                        ty.as_h10().unwrap(),
                    )
                    .await;
            }
        }

        let bt_sensor = Arc::new(self);
        let event_sensor = Arc::clone(&bt_sensor);

        // bluetooth task
        tracing::info!("starting bluetooth task");
        let (bt_tx, mut bt_rx) = mpsc::channel(128);
        tokio::task::spawn(async move {
            let device = bt_sensor.ble_device.as_ref().unwrap();
            let mut notification_stream = device.notifications().await?;

            while let Some(data) = notification_stream.next().await {
                tracing::debug!("received bluetooth data: {data:?}");
                if data.uuid == NotifyUuid::BatteryLevel.into() {
                    let battery = data.value[0];
                    let Ok(_) = bt_tx.send(BluetoothEvent::Battery(battery)).await else { break };
                } else if data.uuid == NotifyUuid::HeartMeasurement.into() {
                    let hr = HeartRate::new(data.value)?;
                    let Ok(_) = bt_tx.send(BluetoothEvent::Hr(hr)).await else { break };
                } else if data.uuid == NotifyUuid::MeasurementData.into() {
                    let pmd = PmdRead::new(data.value)?;
                    let Ok(_) = bt_tx.send(BluetoothEvent::Measurement(pmd)).await else { break };
                }
            }

            Ok::<_, Error>(())
        });

        // event task:
        // handles events and bluetooth data so that no data is lost
        tracing::info!("starting event task");
        let (event_tx, mut event_rx) = mpsc::channel(4);
        tokio::task::spawn(async move {
            loop {
                tokio::select! {
                    Some(data) = bt_rx.recv() => {
                        tracing::debug!("received bt channel message");
                        use BluetoothEvent::*;
                        match data {
                            Battery(b) => handler.battery_update(b).await,
                            Hr(h) => handler.heart_rate_update(h).await,
                            Measurement(m) => handler.measurement_update(m).await,
                        }
                    }
                    Some(event) = event_rx.recv() => {
                        tracing::debug!("received event: {event:?}");
                        match event {
                            Event::Stop => {
                                break;
                            }
                            Event::Add { ty, ret } => {
                                let res = event_sensor.get_pmd_response(ControlPointCommand::RequestMeasurementStart, ty).await;
                                let _ = ret.send(res);
                            }
                            Event::Remove { ty, ret } => {
                                let res = event_sensor.get_pmd_response(ControlPointCommand::StopMeasurement, ty).await;
                                let _ = ret.send(res);
                            }
                        }
                    }
                    else => {
                        break;
                    }
                }
            }
        });

        PolarHandle::new(event_tx)
    }
}

impl<L: Level + Connected> PolarSensor<L> {
    #[instrument(skip(self))]
    async fn subscribe(&self, ty: NotifyStream) -> PolarResult<()> {
        tracing::info!("subscribing to {:?}", ty);
        let device = self.ble_device.as_ref().expect("device already connected");

        let characteristics = device.characteristics();
        let characteristic = characteristics
            .iter()
            .find(|c| c.uuid == ty.into())
            .ok_or(Error::CharacteristicNotFound)?;

        device.subscribe(&characteristic).await?;

        Ok(())
    }

    #[instrument(skip(self))]
    async fn unsubscribe(&self, ty: NotifyStream) -> PolarResult<()> {
        tracing::info!("unsubscribing to {ty:?}");
        let device = self.ble_device.as_ref().unwrap();

        let characteristics = device.characteristics();
        let characteristic = characteristics
            .iter()
            .find(|c| c.uuid == ty.into())
            .ok_or(Error::CharacteristicNotFound)?;

        device.unsubscribe(&characteristic).await?;

        Ok(())
    }

    /// Request the sdk features for your H10
    #[instrument(skip(self))]
    pub async fn features(&self) -> PolarResult<SupportedFeatures> {
        tracing::info!("fetching supported features");
        let controller = self.control_point.as_ref().unwrap();
        let device = self.ble_device.as_ref().unwrap();

        let feat = SupportedFeatures::new(controller.read(&device).await?[1]);
        Ok(feat)
    }

    #[instrument(skip(self))]
    async fn get_pmd_response(
        &self,
        command: ControlPointCommand,
        ty: H10MeasurementType,
    ) -> PolarResult<ControlResponse> {
        tracing::info!("sending control point message: {command:?} ({ty:?})");
        // start measurement and capture response
        let device = self.ble_device.as_ref().unwrap();
        self.subscribe(NotifyStream::MeasurementCP).await?;
        let mut notification_stream = device.notifications().await.map_err(Error::BleError)?;

        // Execute write to PMD command point
        match command {
            ControlPointCommand::Null => return Err(Error::NullCommand),
            ControlPointCommand::GetMeasurementSettings => self.internal_settings(ty).await?,
            ControlPointCommand::RequestMeasurementStart => self.start_measurement(ty).await?,
            ControlPointCommand::StopMeasurement => self.stop_measurement(ty).await?,
        };

        let response = loop {
            let Some(data) = notification_stream.next().await else {
                return Err(Error::NoControlPointResponse);
            };
            if data.uuid == NotifyUuid::MeasurementCP.into() {
                break ControlResponse::new(data.value).await?;
            }
        };
        self.unsubscribe(NotifyStream::MeasurementCP).await?;
        tracing::info!("response: {response:?}");

        Ok(response)
    }

    async fn internal_settings(&self, ty: H10MeasurementType) -> PolarResult<()> {
        let controller = self.control_point.as_ref().unwrap();
        let device = self.ble_device.as_ref().unwrap();

        controller
            .send_command(&device, [1, ty.as_u8()].to_vec())
            .await?;

        Ok(())
    }

    async fn start_measurement(&self, ty: H10MeasurementType) -> PolarResult<()> {
        let controller = self.control_point.as_ref().unwrap();
        let device = self.ble_device.as_ref().unwrap();

        let mut command = vec![0x02, ty.as_u8()];

        // Add range and resolution characteristic for acceleration only
        match ty {
            H10MeasurementType::Acc => {
                // Range
                command.push(0x02);
                command.push(0x01);
                command.push(self.range);
                command.push(0x00);

                // Sample rate
                command.push(0x00);
                command.push(0x01);
                command.push(self.sample_rate);
                command.push(0x00);

                // Resolution
                command.push(0x01);
                command.push(0x01);
                command.push(0x10);
                command.push(0x00);
            }
            H10MeasurementType::Ecg => {
                // Sample rate
                command.push(0x00);
                command.push(0x01);
                command.push(0x82);
                command.push(0x00);

                // Resolution
                command.push(0x01);
                command.push(0x01);
                command.push(0x0e);
                command.push(0x00);
            }
        }
        controller.send_command(device, command).await?;
        Ok(())
    }

    async fn stop_measurement(&self, ty: H10MeasurementType) -> PolarResult<()> {
        let controller = self.control_point.as_ref().unwrap();
        let device = self.ble_device.as_ref().unwrap();

        controller
            .send_command(&device, [3, ty.as_u8()].to_vec())
            .await?;

        Ok(())
    }
}

/// Handle to the [`PolarSensor`] that is running an event loop
#[derive(Clone)]
pub struct PolarHandle {
    sender: mpsc::Sender<Event>,
}

impl PolarHandle {
    fn new(sender: mpsc::Sender<Event>) -> Self {
        Self { sender }
    }

    /// Stop the [`PolarSensor`]
    #[instrument(skip(self))]
    pub async fn stop(self) {
        tracing::info!("stopping sensor");
        let _ = self.sender.send(Event::Stop).await;
    }

    /// Start measuring an [`H10MeasurementType`] on the running sensor.
    ///
    /// Returns [`None`] if the event handler stopped running.
    #[instrument(skip(self))]
    pub async fn add(&self, ty: H10MeasurementType) -> Option<PolarResult<ControlResponse>> {
        tracing::info!("adding type: {ty:?}");
        let (ret, rx) = oneshot::channel();
        let _ = self.sender.send(Event::Add { ty, ret });

        rx.await.ok()
    }

    /// Stop measuring an [`H10MeasurementType`] on the running sensor.
    ///
    /// Returns [`None`] if the event handler stopped running.
    #[instrument(skip(self))]
    pub async fn remove(&self, ty: H10MeasurementType) -> Option<PolarResult<ControlResponse>> {
        tracing::info!("removing type: {ty:?}");
        let (ret, rx) = oneshot::channel();
        let _ = self.sender.send(Event::Remove { ty, ret });

        rx.await.ok()
    }
}

/// Type of events to send to the event loop of [`PolarSensor`]
#[derive(Debug)]
enum Event {
    /// Stop the event loop
    Stop,
    /// Start listening to an event type
    Add {
        /// type of measurement to add
        ty: H10MeasurementType,
        /// channel to receive return value
        ret: oneshot::Sender<PolarResult<ControlResponse>>,
    },
    /// Stop listening to an event type
    Remove {
        /// type of measurement to remove
        ty: H10MeasurementType,
        /// channel to receive return value
        ret: oneshot::Sender<PolarResult<ControlResponse>>,
    },
}

/// Bluetooth data received from the sensor
#[derive(Debug, Clone)]
enum BluetoothEvent {
    Battery(u8),
    Hr(HeartRate),
    Measurement(PmdRead),
}

// === Trait Sealing ===
// So crate users can't implement [`Level`] on any type to make weird [`PolarSensor`]s

/// Marker for the construction level of a [`PolarSensor`]
pub trait Level: internal::Level {}
impl<L> Level for L where L: internal::Level {}

/// Marker for [`PolarSensor`] connected over Bluetooth
pub trait Connected: internal::Level {}

mod internal {
    /// Marker for level of [`crate::v2::PolarSensor`]
    pub trait Level {}
}

/// [`PolarSensor`] level for connecting to your device
pub struct Bluetooth;

impl internal::Level for Bluetooth {}

/// [`PolarSensor`] level for registering data types to listen for
#[derive(Default)]
pub struct Configure {
    /// Is subscribed to battery stream
    pub battery: bool,
    /// Is subscribed to heart rate stream
    pub heart_rate: bool,
    /// Is subscribed to control point stream
    pub measurement_cp: bool,
    /// Is subscribed to PMD data stream
    pub measurement_data: bool,
}

impl internal::Level for Configure {}
impl Connected for Configure {}

/// [`PolarSensor`] level for starting the event loop
pub struct EventLoop;

impl internal::Level for EventLoop {}
impl Connected for EventLoop {}

/// [`PolarSensor`] level for a running event loop
pub struct Running;

impl internal::Level for Running {}
impl Connected for Running {}
