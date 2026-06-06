//! Event-driven audio device monitor using Windows Core Audio (MMDevice) COM API.
//! Registers an `IMMNotificationClient` callback to receive default audio device
//! changes and re-initialize the mpv audio output context immediately.

use std::sync::Weak;
use windows::core::{implement, PCWSTR, Result};
use windows::Win32::Media::Audio::{
    eConsole, eMultimedia, eRender, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, EDataFlow, ERole, DEVICE_STATE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::Foundation::PROPERTYKEY;

use crate::player::Player;

#[implement(IMMNotificationClient)]
struct AudioDeviceNotificationClient {
    player: Weak<Player>,
}

impl AudioDeviceNotificationClient {
    fn new(player: Weak<Player>) -> Self {
        Self { player }
    }
}

impl IMMNotificationClient_Impl for AudioDeviceNotificationClient_Impl {
    fn OnDeviceStateChanged(&self, _pwstrdeviceid: &PCWSTR, _dwnewstate: DEVICE_STATE) -> Result<()> {
        Ok(())
    }

    fn OnDeviceAdded(&self, _pwstrdeviceid: &PCWSTR) -> Result<()> {
        Ok(())
    }

    fn OnDeviceRemoved(&self, _pwstrdeviceid: &PCWSTR) -> Result<()> {
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        pwstrdefaultdeviceid: &PCWSTR,
    ) -> Result<()> {
        // We only care about playback (eRender) devices and console/multimedia roles.
        if flow == eRender && (role == eConsole || role == eMultimedia) {
            let id_str = unsafe { pwstrdefaultdeviceid.to_string().unwrap_or_else(|_| "unknown".to_string()) };
            if let Some(player) = self.player.upgrade() {
                log::info!("Default audio playback device changed in Windows to {}. Reloading mpv audio output...", id_str);
                std::thread::spawn(move || {
                    player.reload_audio_output();
                });
            }
        }
        Ok(())
    }

    fn OnPropertyValueChanged(&self, _pwstrdeviceid: &PCWSTR, _key: &PROPERTYKEY) -> Result<()> {
        Ok(())
    }
}

/// A handle to the registered audio endpoint change notification callback.
/// Unregisters the callback when dropped.
pub struct AudioMonitor {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
}

impl AudioMonitor {
    /// Create a new `AudioMonitor` and register it to listen for endpoint changes.
    pub fn start(player: Weak<Player>) -> Result<Self> {
        unsafe {
            // Note: COM is initialized as STA on the main thread in compositor.rs.
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let client: IMMNotificationClient = AudioDeviceNotificationClient::new(player).into();
            
            enumerator.RegisterEndpointNotificationCallback(&client)?;
            log::info!("Audio device change monitor successfully registered.");
            
            Ok(Self { enumerator, client })
        }
    }
}

impl Drop for AudioMonitor {
    fn drop(&mut self) {
        unsafe {
            if let Err(e) = self.enumerator.UnregisterEndpointNotificationCallback(&self.client) {
                log::error!("failed to unregister audio notification callback: {:?}", e);
            } else {
                log::info!("Audio device change monitor unregistered.");
            }
        }
    }
}
