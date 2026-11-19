//! `SessionSource` on WASAPI, with events. Per Microsoft's guidance the order is: register for endpoint changes
//! (`IMMNotificationClient`), then for each capture endpoint register for new sessions
//! (`IAudioSessionNotification`) before enumerating its sessions (the enumeration also turns the notifications on),
//! and register for each session's state changes (`IAudioSessionEvents`) before reading its state. Callbacks only
//! send `Event`s into the monitor loop; anything that fails there becomes a `Doubt`. Any COM failure in a snapshot
//! fails the whole snapshot. Everything runs in the multithreaded apartment, which the caller enters.

use crate::sessions::{Event, Req, Session, SessionSource, Snapshot, single_process};
use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{CloseHandle, PROPERTYKEY};
use windows::Win32::Media::Audio::{
    AudioSessionDisconnectReason, AudioSessionState, AudioSessionStateActive, DEVICE_STATE,
    DEVICE_STATE_ACTIVE, EDataFlow, ERole, IAudioSessionControl, IAudioSessionControl2,
    IAudioSessionEvents, IAudioSessionEvents_Impl, IAudioSessionManager2,
    IAudioSessionNotification, IAudioSessionNotification_Impl, IMMDeviceEnumerator,
    IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator, eCapture,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{BOOL, GUID, Interface, PCWSTR, PWSTR, Ref, Result as ComResult, implement};
use zora_plugin::clock::now_ns;

/// A COM interface used from several threads of the multithreaded apartment, where that is allowed.
struct Mta<T>(T);
// SAFETY: every thread that touches these (the monitor loop and WASAPI's callback threads) is in the MTA, where
// interface pointers may be used from any thread.
unsafe impl<T> Send for Mta<T> {}

struct Watched {
    control: IAudioSessionControl,
    events: IAudioSessionEvents,
}

/// What callbacks share with the monitor loop.
struct Shared {
    tx: Sender<Req>,
    sessions: Mutex<HashMap<String, Mta<Watched>>>,
}

impl Shared {
    fn send(&self, e: Event) {
        let _ = self.tx.send(Req::Event(e));
    }

    fn doubt(&self, reason: impl Into<String>) {
        self.send(Event::Doubt {
            reason: reason.into(),
            at: now_ns(),
        });
    }

    /// Registers for the session's state changes (once), then reads it.
    fn watch(self: &Arc<Self>, control: IAudioSessionControl) -> ComResult<Session> {
        // SAFETY: COM calls on a live session control; the identifier string is freed here.
        unsafe {
            let c2: IAudioSessionControl2 = control.cast()?;
            let id = c2.GetSessionInstanceIdentifier()?;
            let key = id.to_string().unwrap_or_default();
            CoTaskMemFree(Some(id.0 as *const _));
            let mut pid = 0u32;
            // The raw call keeps AUDCLNT_S_NO_SINGLE_PROCESS, a success code the wrapper would drop.
            let hr = (Interface::vtable(&c2).GetProcessId)(Interface::as_raw(&c2), &mut pid);
            let image = single_process(hr.0, pid).and_then(image_of);
            let mut map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            if !map.contains_key(&key) {
                let events: IAudioSessionEvents = SessionEvents {
                    shared: self.clone(),
                    key: key.clone(),
                    image: image.clone(),
                }
                .into();
                control.RegisterAudioSessionNotification(&events)?;
                map.insert(
                    key.clone(),
                    Mta(Watched {
                        control: control.clone(),
                        events,
                    }),
                );
            }
            drop(map);
            let active = control.GetState()? == AudioSessionStateActive;
            Ok(Session { key, image, active })
        }
    }

    fn unwatch(&self, keep: impl Fn(&str) -> bool) {
        let mut map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|k, w| {
            if !keep(k) {
                // SAFETY: unregisters the callback registered in `watch`.
                let _ = unsafe { w.0.control.UnregisterAudioSessionNotification(&w.0.events) };
            }
            keep(k)
        });
    }
}

#[implement(IMMNotificationClient)]
struct Devices(Arc<Shared>);

impl IMMNotificationClient_Impl for Devices_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> ComResult<()> {
        self.0.doubt("an audio device changed state");
        Ok(())
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> ComResult<()> {
        self.0.doubt("an audio device was added");
        Ok(())
    }
    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> ComResult<()> {
        self.0.doubt("an audio device was removed");
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, _: EDataFlow, _: ERole, _: &PCWSTR) -> ComResult<()> {
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> ComResult<()> {
        Ok(())
    }
}

#[implement(IAudioSessionNotification)]
struct NewSessions(Arc<Shared>);

impl IAudioSessionNotification_Impl for NewSessions_Impl {
    fn OnSessionCreated(&self, session: Ref<IAudioSessionControl>) -> ComResult<()> {
        let watched = session.ok().and_then(|c| self.0.watch(c.clone()));
        match watched {
            Ok(s) => self.0.send(Event::Session {
                key: s.key,
                image: s.image,
                active: s.active,
                at: now_ns(),
            }),
            Err(e) => self
                .0
                .doubt(format!("a new audio session couldn't be watched: {e}")),
        }
        Ok(())
    }
}

#[implement(IAudioSessionEvents)]
struct SessionEvents {
    shared: Arc<Shared>,
    key: String,
    image: Option<String>,
}

impl IAudioSessionEvents_Impl for SessionEvents_Impl {
    fn OnDisplayNameChanged(&self, _: &PCWSTR, _: *const GUID) -> ComResult<()> {
        Ok(())
    }
    fn OnIconPathChanged(&self, _: &PCWSTR, _: *const GUID) -> ComResult<()> {
        Ok(())
    }
    fn OnSimpleVolumeChanged(&self, _: f32, _: BOOL, _: *const GUID) -> ComResult<()> {
        Ok(())
    }
    fn OnChannelVolumeChanged(
        &self,
        _: u32,
        _: *const f32,
        _: u32,
        _: *const GUID,
    ) -> ComResult<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(&self, _: *const GUID, _: *const GUID) -> ComResult<()> {
        Ok(())
    }
    fn OnStateChanged(&self, state: AudioSessionState) -> ComResult<()> {
        self.shared.send(Event::Session {
            key: self.key.clone(),
            image: self.image.clone(),
            active: state == AudioSessionStateActive,
            at: now_ns(),
        });
        Ok(())
    }
    fn OnSessionDisconnected(&self, _: AudioSessionDisconnectReason) -> ComResult<()> {
        self.shared.send(Event::Gone {
            key: self.key.clone(),
            at: now_ns(),
        });
        Ok(())
    }
}

pub struct Wasapi {
    devices: IMMDeviceEnumerator,
    device_events: IMMNotificationClient,
    endpoints: HashMap<String, (IAudioSessionManager2, IAudioSessionNotification)>,
    shared: Arc<Shared>,
}

impl Wasapi {
    /// Creates the device enumerator and registers for endpoint changes. The calling thread must be in the MTA.
    pub fn new(tx: Sender<Req>) -> Result<Wasapi, String> {
        let shared = Arc::new(Shared {
            tx,
            sessions: Mutex::new(HashMap::new()),
        });
        // SAFETY: COM calls on interfaces created here.
        unsafe {
            let devices: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| e.to_string())?;
            let device_events: IMMNotificationClient = Devices(shared.clone()).into();
            devices
                .RegisterEndpointNotificationCallback(&device_events)
                .map_err(|e| e.to_string())?;
            Ok(Wasapi {
                devices,
                device_events,
                endpoints: HashMap::new(),
                shared,
            })
        }
    }

    fn read(&mut self) -> ComResult<Snapshot> {
        let mut snap = Snapshot::default();
        // SAFETY: COM calls on interfaces this object owns; returned strings are freed with CoTaskMemFree.
        unsafe {
            let all = self
                .devices
                .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
            for i in 0..all.GetCount()? {
                let device = all.Item(i)?;
                let raw = device.GetId()?;
                let id = raw.to_string().unwrap_or_default();
                CoTaskMemFree(Some(raw.0 as *const _));
                if !self.endpoints.contains_key(&id) {
                    let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
                    let created: IAudioSessionNotification =
                        NewSessions(self.shared.clone()).into();
                    // Before the first enumeration, so no session created meanwhile is missed.
                    manager.RegisterSessionNotification(&created)?;
                    self.endpoints.insert(id.clone(), (manager, created));
                }
                let (manager, _) = &self.endpoints[&id];
                let sessions = manager.GetSessionEnumerator()?;
                for j in 0..sessions.GetCount()? {
                    snap.sessions
                        .push(self.shared.watch(sessions.GetSession(j)?)?);
                }
                snap.devices.push(id);
            }
        }
        self.endpoints.retain(|id, (manager, created)| {
            let keep = snap.devices.contains(id);
            if !keep {
                // SAFETY: unregisters the callback registered above.
                let _ = unsafe { manager.UnregisterSessionNotification(&*created) };
            }
            keep
        });
        let keys: Vec<&str> = snap.sessions.iter().map(|s| s.key.as_str()).collect();
        self.shared.unwatch(|k| keys.contains(&k));
        Ok(snap)
    }
}

impl Drop for Wasapi {
    fn drop(&mut self) {
        self.shared.unwatch(|_| false);
        // SAFETY: unregisters the callbacks registered in `new` and `read`.
        unsafe {
            for (manager, created) in self.endpoints.values() {
                let _ = manager.UnregisterSessionNotification(created);
            }
            let _ = self
                .devices
                .UnregisterEndpointNotificationCallback(&self.device_events);
        }
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

/// Creates the enumerator (and its registrations) on first use and again after any failure, so a restarted
/// audio service is survived. The thread must already be in the MTA.
pub struct Reconnecting {
    tx: Sender<Req>,
    inner: Option<Wasapi>,
}

impl Reconnecting {
    pub fn new(tx: Sender<Req>) -> Reconnecting {
        Reconnecting { tx, inner: None }
    }
}

impl SessionSource for Reconnecting {
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        if self.inner.is_none() {
            self.inner = Some(Wasapi::new(self.tx.clone())?);
        }
        let result = match self.inner.as_mut() {
            Some(w) => w.snapshot(),
            None => Err("no enumerator".into()),
        };
        if result.is_err() {
            self.inner = None;
        }
        result
    }
}
