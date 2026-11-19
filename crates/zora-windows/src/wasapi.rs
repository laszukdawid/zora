//! `SessionSource` on WASAPI: every active capture endpoint, its audio sessions (IAudioSessionManager2), each
//! session's state and its process image. Any COM failure fails the whole snapshot.

use crate::sessions::{Session, SessionSource, Snapshot};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, MMDeviceEnumerator, eCapture,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{Interface, PWSTR};

pub struct Wasapi {
    devices: IMMDeviceEnumerator,
}

impl Wasapi {
    /// Joins the multithreaded COM apartment on this thread and creates the device enumerator.
    pub fn new() -> Result<Wasapi, String> {
        // SAFETY: plain COM setup on the calling thread; S_FALSE (already initialized) is fine.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let devices: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| e.to_string())?;
            Ok(Wasapi { devices })
        }
    }

    fn read(&self) -> windows::core::Result<Snapshot> {
        let mut snap = Snapshot::default();
        // SAFETY: COM calls on interfaces this object owns; returned strings are freed with CoTaskMemFree.
        unsafe {
            let all = self
                .devices
                .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
            for i in 0..all.GetCount()? {
                let device = all.Item(i)?;
                let id = device.GetId()?;
                snap.devices.push(id.to_string().unwrap_or_default());
                CoTaskMemFree(Some(id.0 as *const _));
                let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
                let sessions = manager.GetSessionEnumerator()?;
                for j in 0..sessions.GetCount()? {
                    let control = sessions.GetSession(j)?;
                    let active = control.GetState()? == AudioSessionStateActive;
                    let image = control
                        .cast::<IAudioSessionControl2>()
                        .ok()
                        .and_then(|c| c.GetProcessId().ok())
                        .filter(|pid| *pid != 0)
                        .and_then(image_of);
                    snap.sessions.push(Session { image, active });
                }
            }
        }
        Ok(snap)
    }
}

impl SessionSource for Wasapi {
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        self.read().map_err(|e| e.to_string())
    }
}

/// The full image path of a running process, or `None` when it can't be read.
fn image_of(pid: u32) -> Option<String> {
    // SAFETY: the handle is closed below; the buffer and its length match.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        ok.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Creates the enumerator on first use and again after any failure, so a COM server restart is survived.
#[derive(Default)]
pub struct Reconnecting(Option<Wasapi>);

impl SessionSource for Reconnecting {
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        if self.0.is_none() {
            self.0 = Some(Wasapi::new()?);
        }
        let result = self
            .0
            .as_mut()
            .map(|w| w.snapshot())
            .unwrap_or(Err("no enumerator".into()));
        if result.is_err() {
            self.0 = None;
        }
        result
    }
}
