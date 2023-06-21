use crate::middleware::usbboot::{FlashProgress, FlashStatus};
use crate::middleware::{
    app_persistency::ApplicationPersistency, event_listener::EventListener,
    pin_controller::PinController, usbboot, NodeId, UsbMode, UsbRoute,
};
use anyhow::{ensure, Context};
use evdev::Key;
use log::debug;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::sleep;

/// Stores which slots are actually used. This information is used to determine
/// for instance, which nodes need to be powered on, when such command is given
const ACTIVATED_NODES_KEY: &str = "activated_nodes";
/// stores to which node the usb multiplexer is configured to.
const USB_NODE_KEY: &str = "usb_node";
const USB_ROUTE_KEY: &str = "usb_route";
const USB_MODE_KEY: &str = "usb_mode";

const REBOOT_DELAY: Duration = Duration::from_millis(500);

const SUPPORTED_DEVICES: [UsbMassStorageProperty; 1] = [UsbMassStorageProperty {
    _name: "Raspberry Pi CM4",
    vid: 0x0a5c,
    pid: 0x2711,
    disk_prefix: Some("RPi-MSD-"),
}];

#[derive(Debug)]
struct UsbMassStorageProperty {
    pub _name: &'static str,
    pub vid: u16,
    pub pid: u16,
    pub disk_prefix: Option<&'static str>,
}

#[derive(Debug)]
pub struct BmcApplication {
    pin_controller: PinController,
    app_db: ApplicationPersistency,
    power_state: Mutex<u8>,
}

impl BmcApplication {
    pub async fn new() -> anyhow::Result<Arc<Self>> {
        let pin_controller = PinController::new()?;
        let app_db = ApplicationPersistency::new().await?;

        let instance = Arc::new(Self {
            pin_controller,
            app_db,
            power_state: Mutex::new(0),
        });

        instance.initialize().await?;
        Self::run_event_listener(instance.clone())?;
        Ok(instance)
    }

    fn run_event_listener(instance: Arc<BmcApplication>) -> anyhow::Result<()> {
        // start listening for device events.
        EventListener::new(
            (instance, Option::<oneshot::Sender<()>>::None),
            "/dev/input/event0",
        )
        .add_action(Key::KEY_1, 1, |(app, s)| {
            let (sender, receiver) = oneshot::channel();
            *s = Some(sender);

            let bmc = app.clone();
            tokio::spawn(async move {
                let long_press = tokio::time::timeout(Duration::from_secs(3), receiver)
                    .await
                    .is_err();
                Self::toggle_power_states(bmc, long_press).await
            });
        })
        .add_action(Key::KEY_1, 0, |(_, sender)| {
            let _ = sender.take().and_then(|s| s.send(()).ok());
        })
        .add_action(Key::KEY_POWER, 1, |(app, _)| {
            tokio::spawn(Self::toggle_power_states(app.clone(), false));
        })
        .add_action(Key::KEY_RESTART, 1, |_| {
            tokio::spawn(reboot());
        })
        .run()
        .context("event_listener error")
    }

    async fn toggle_power_states(
        app: Arc<BmcApplication>,
        mut reset_activation: bool,
    ) -> anyhow::Result<()> {
        let lock = app.power_state.lock().await;
        if *lock == 0 {
            // For first time use, when the user didnt powered any nodes yet.
            // Activate them all.
            reset_activation = true;
        }

        let mut node_values = *lock;
        drop(lock);

        if reset_activation {
            let value = if node_values < 15 { 0b1111 } else { 0b0000 };
            app.app_db.set(ACTIVATED_NODES_KEY, value).await?;
        }
        node_values = !node_values;

        app.power_node(node_values, 0b1111).await
    }

    async fn initialize(&self) -> anyhow::Result<()> {
        self.initialize_usb_mode().await?;
        self.initialize_power().await
    }

    async fn initialize_power(&self) -> anyhow::Result<()> {
        // power on nodes
        if let Ok(enabled_nodes) = self.app_db.get::<u8>(ACTIVATED_NODES_KEY).await {
            self.power_node(enabled_nodes, 0b1111).await
        } else {
            // default, given a new app persistency
            self.app_db.set::<u8>(ACTIVATED_NODES_KEY, 0).await
        }
    }

    async fn initialize_usb_mode(&self) -> std::io::Result<()> {
        let node = self
            .app_db
            .get::<NodeId>(USB_NODE_KEY)
            .await
            .unwrap_or(NodeId::Node1);
        let res = self.pin_controller.select_usb(node);

        let route = self
            .app_db
            .get::<UsbRoute>(USB_ROUTE_KEY)
            .await
            .unwrap_or(UsbRoute::UsbA);
        let res2 = self.pin_controller.set_usb_route(route);

        let mode = self.app_db.get::<u8>(USB_MODE_KEY).await.unwrap_or(0b1111);
        let res3 = self.pin_controller.inner_set_usb_mode(mode);

        res.and(res2).and(res3)
    }

    /// Helper function that returns the new state of ATX power
    fn need_atx_change(current_node_state: u8, next_node_state: u8) -> Option<bool> {
        if current_node_state == 0 && next_node_state > 0 {
            // power down
            Some(true)
        } else if current_node_state > 0 && next_node_state == 0 {
            // power up
            Some(false)
        } else {
            // dont do anything
            None
        }
    }

    pub async fn get_node_power(&self, node: NodeId) -> anyhow::Result<bool> {
        let state = self.power_state.lock().await;
        Ok(*state & node.to_bitfield() != 0)
    }

    /// This function is used to active a given node. Call this function if a
    /// module is inserted at that slot. Failing to call this method means that
    /// this slot is not considered for power up and power down commands.
    pub async fn activate_slot(&self, node: NodeId, on: bool) -> anyhow::Result<()> {
        ensure!(node.to_bitfield() != 0);

        let mask = node.to_bitfield();
        let mut bits = node.to_bitfield();

        if !on {
            bits = !bits;
        }

        let mut state = self.app_db.get::<u8>(ACTIVATED_NODES_KEY).await?;
        state = (state & !mask) | (bits & mask);

        self.app_db.set::<u8>(ACTIVATED_NODES_KEY, state).await?;
        debug!("node activated bits updated. new value= {:#04b}", state);

        // also update the actual power state accordingly
        self.power_node(bits, mask).await
    }

    pub async fn power_on(&self) -> anyhow::Result<()> {
        self.power_node(0b1111, 0b1111).await
    }

    pub async fn power_off(&self) -> anyhow::Result<()> {
        self.power_node(0b0000, 0b1111).await
    }

    async fn power_node(&self, nodes: u8, mask: u8) -> anyhow::Result<()> {
        let activated = self.app_db.get::<u8>(ACTIVATED_NODES_KEY).await?;
        let mut current_power_state = self.power_state.lock().await;
        let new_power_state = Self::power_logic(nodes, mask, activated, *current_power_state);
        if new_power_state == *current_power_state {
            debug!(
                "requested powerstate {:#04b} is already active. activated nodes={:#04b}",
                *current_power_state, activated
            );
            return Ok(());
        };

        if let Some(on) = Self::need_atx_change(*current_power_state, new_power_state) {
            debug!("changing state of atx to {}", on);
            self.pin_controller.set_atx_power(on).await?;
            let led_val = if on { b"1" } else { b"0" };
            tokio::fs::write("/sys/class/leds/fp:sys/brightness", led_val).await?;
        }

        debug!(
            "applying change in power state. current state={:#04b}, new state={:#04b}",
            *current_power_state, new_power_state
        );

        self.pin_controller
            .set_power_node(*current_power_state, new_power_state)
            .await
            .context("pin controller error")?;

        *current_power_state = new_power_state;
        Ok(())
    }

    /// determines the new `power_state` given the inputs
    ///
    /// # Arguments
    ///
    /// * `node_values`   set values of nodes, use in combination with `node_mask`
    /// * `node_mask`     select which values to update
    /// * `activated_nodes` a mask that has precendense over the node mask, if a given node is not
    /// activated, setting a value will be ignored
    /// * `current_state`  the current state of nodes
    fn power_logic(node_values: u8, node_mask: u8, activated_nodes: u8, current_state: u8) -> u8 {
        // make sure that only activated nodes are allowed to be on
        let mut new_power_state = current_state & activated_nodes;

        // only set nodes that are allowed to be set. i.e. that are activated.
        let mask = node_mask & activated_nodes;
        if mask != 0 {
            new_power_state = (new_power_state & !mask) | (node_values & mask);
        }

        new_power_state
    }

    pub async fn usb_mode(&self, mode: UsbMode, node: NodeId) -> anyhow::Result<()> {
        self.pin_controller.select_usb(node)?;
        self.app_db.set(USB_NODE_KEY, node).await?;

        self.pin_controller.set_usb_route(UsbRoute::UsbA)?;
        self.app_db.set(USB_ROUTE_KEY, UsbRoute::UsbA).await?;

        self.set_usb_mode(node, mode).await?;

        // Hack: as in the previous version of the firmware, set RPIBOOT pins of a node when the
        // selected mode is "device", because users execute a command such as `tpi -n 1 -u device`
        // and expect device to be flash-able via rpiboot.
        match mode {
            UsbMode::Host => self.pin_controller.clear_usb_boot()?,
            UsbMode::Device => self.pin_controller.set_usb_boot(node)?,
        }

        Ok(())
    }

    async fn set_usb_mode(&self, node: NodeId, mode: UsbMode) -> anyhow::Result<()> {
        let prev_mode = self.app_db.get::<u8>(USB_MODE_KEY).await.unwrap_or(0b1111);
        let new_mode = self.pin_controller.set_usb_mode(node, mode, prev_mode)?;

        self.app_db.set(USB_MODE_KEY, new_mode).await
    }

    pub async fn rtl_reset(&self) -> anyhow::Result<()> {
        self.pin_controller.rtl_reset().await.context("rtl error")
    }

    pub async fn set_node_in_msd(
        &self,
        node: NodeId,
        router: UsbRoute,
        progress_sender: mpsc::Sender<FlashProgress>,
    ) -> anyhow::Result<PathBuf> {
        let mut progress_state = FlashProgress {
            message: String::new(),
            status: FlashStatus::Idle,
        };

        progress_state.message = format!("Powering off node {}...", node as u8 + 1);
        progress_state.status = FlashStatus::Progress {
            read_percent: 0,
            est_minutes: u64::MAX,
            est_seconds: u64::MAX,
        };
        progress_sender.send(progress_state.clone()).await?;

        self.activate_slot(node, false).await?;
        self.pin_controller.clear_usb_boot()?;

        sleep(REBOOT_DELAY).await;

        self.pin_controller.select_usb(node)?;
        self.pin_controller.set_usb_boot(node)?;
        self.pin_controller.set_usb_route(router)?;

        self.set_usb_mode(node, UsbMode::Device).await?;

        progress_state.message = String::from("Prerequisite settings toggled, powering on...");
        progress_sender.send(progress_state.clone()).await?;

        self.activate_slot(node, true).await?;

        sleep(Duration::from_secs(2)).await;

        progress_state.message = String::from("Checking for presence of a USB device...");
        progress_sender.send(progress_state.clone()).await?;

        let matches =
            usbboot::get_serials_for_vid_pid(SUPPORTED_DEVICES.iter().map(|d| (d.vid, d.pid)))?;
        usbboot::verify_one_device(&matches).map_err(|e| {
            progress_sender
                .try_send(FlashProgress {
                    status: FlashStatus::Error(e),
                    message: String::new(),
                })
                .unwrap();
            e
        })?;

        progress_state.message = String::from("Rebooting as a USB mass storage device...");
        progress_sender.send(progress_state.clone()).await?;

        usbboot::boot_node_to_msd(node)?;

        sleep(Duration::from_secs(3)).await;
        progress_state.message = String::from("Checking for presence of a device file...");
        progress_sender.send(progress_state.clone()).await?;

        usbboot::get_device_path(SUPPORTED_DEVICES.iter().filter_map(|d| d.disk_prefix))
            .await
            .context("error getting device path")
    }

    pub async fn flash_node(
        self: Arc<BmcApplication>,
        node: NodeId,
        image_path: PathBuf,
        progress_sender: mpsc::Sender<FlashProgress>,
    ) -> anyhow::Result<()> {
        let device_path = self
            .set_node_in_msd(node, UsbRoute::BMC, progress_sender.clone())
            .await?;

        let mut progress_state = FlashProgress {
            message: String::new(),
            status: FlashStatus::Idle,
        };
        progress_state.message = format!("Writing {:?} to {:?}", image_path, device_path);
        progress_sender.send(progress_state.clone()).await?;

        let (img_len, img_checksum) =
            usbboot::write_to_device(image_path, &device_path, &progress_sender).await?;

        progress_state.message = String::from("Verifying checksum...");
        progress_sender.send(progress_state.clone()).await?;

        usbboot::verify_checksum(img_checksum, img_len, &device_path, &progress_sender).await?;

        progress_state.message = String::from("Flashing successful, restarting device...");
        progress_sender.send(progress_state.clone()).await?;

        self.activate_slot(node, false).await?;
        self.usb_mode(UsbMode::Host, node).await?;

        sleep(REBOOT_DELAY).await;

        self.activate_slot(node, true).await?;

        progress_state.message = String::from("Done");
        progress_sender.send(progress_state).await?;
        Ok(())
    }

    pub fn clear_usb_boot(&self) -> anyhow::Result<()> {
        self.pin_controller
            .clear_usb_boot()
            .context("error clearing usbboot")
    }
}

async fn reboot() -> anyhow::Result<()> {
    tokio::fs::write("/sys/class/leds/fp:reset/brightness", b"1").await?;
    Command::new("shutdown").args(["-r", "now"]).spawn()?;
    Ok(())
}

#[cfg(test)]
mod test {
    use super::BmcApplication;

    #[test]
    fn test_power_logic_on_off() {
        // turn all actived nodes on
        assert_eq!(
            0b1001,
            BmcApplication::power_logic(0b1111, 0b1111, 0b1001, 0b0)
        );
        // turn all activated nodes on, and reset nodes that are not part of the
        // activated nodes anymore
        assert_eq!(
            0b1001,
            BmcApplication::power_logic(0b1111, 0b1111, 0b1001, 0b1100)
        );
        // turn all nodes off
        assert_eq!(0, BmcApplication::power_logic(0b0, 0b1111, 0b1001, 0b1100));
        // turn all nodes off, but the powerstate was already completely off.
        // hence do nothing.
        assert_eq!(0, BmcApplication::power_logic(0b0, 0b1111, 0b1001, 0b0));
        // turn all activated nodes off, and reset nodes that are not part of the
        // activated nodes anymore
        assert_eq!(0, BmcApplication::power_logic(0b0, 0b1111, 0b0, 0b1100));
        // turning on all nodes, without having activated nodes result into no
        // action.
        assert_eq!(0, BmcApplication::power_logic(0b1111, 0b1111, 0b0, 0b0));
    }

    #[test]
    fn test_individual_nodes() {
        // request to set an individual node which is not activated. Node should
        // not be updated.
        assert_eq!(
            0b1010,
            BmcApplication::power_logic(0b0100, 0b0100, 0b1011, 0b1010)
        );
        // request to set an individual node which is not activated. Node should
        // not be updated. However the change in activation bits should be
        // updated.
        assert_eq!(
            0b1000,
            BmcApplication::power_logic(0b0100, 0b0100, 0b1001, 0b1010)
        );
        //update 2 nodes which are activated. first node is already on
        assert_eq!(
            0b1101,
            BmcApplication::power_logic(0b0101, 0b0101, 0b1101, 0b1001)
        );
        //turn off 2 nodes which are activated.
        assert_eq!(
            0b1000,
            BmcApplication::power_logic(0b0, 0b0101, 0b1101, 0b1101)
        );
    }
}
