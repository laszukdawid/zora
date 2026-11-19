//! `SessionSource` on WASAPI, with events. The order follows Microsoft's guidance: register for endpoint changes
//! (`IMMNotificationClient`), then for each capture endpoint register for new sessions
//! (`IAudioSessionNotification`) before enumerating its sessions (the enumeration also turns the notifications
//! on), and register for each session's state changes (`IAudioSessionEvents`) before reading its state.
//!
//! Callbacks never register, unregister or take a lock: they send events, and `OnSessionCreated` hands an owned
//! reference to the new session to the owner thread (the monitor loop), which subscribes to it and reads it;
//! coverage is unknown through that hand-off. Sessions are kept from creation (or first enumeration) until they
//! disconnect, expire, or their endpoint goes away, not just while an enumeration lists them. Everything runs in
//! the multithreaded apartment, which the caller enters.

use crate::sessions::{Event, Req, Session, SessionSource, Snapshot, single_process};
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use windows::Win32::Foundation::{CloseHandle, PROPERTYKEY};
use windows::Win32::Media::Audio::{
    AudioSessionDisconnectReason, AudioSessionState, AudioSessionStateActive,
    AudioSessionStateExpired, DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow, ERole,
    IAudioSessionControl, IAudioSessionControl2, IAudioSessionEvents, IAudioSessionEvents_Impl,
    IAudioSessionManager2, IAudioSessionNotification, IAudioSessionNotification_Impl,
    IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator,
    eCapture,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{BOOL, GUID, Interface, PCWSTR, PWSTR, Ref, Result as ComResult, implement};
use zora_plugin::clock::{Ns, now_ns};

/// A COM interface handed from a callback thread to the owner thread.
struct Mta<T>(T);
// SAFETY: both threads are in the multithreaded apartment, where interface pointers may be used from any thread.
unsafe impl<T> Send for Mta<T> {}

/// From `OnSessionCreated`: a new session (an AddRef'd reference) and its endpoint.
struct Created {
    control: Mta<IAudioSessionControl>,
    device: String,
}

/// From a session's callbacks: it ended for good; the owner unsubscribes.
struct Retire(String);

/// What callbacks may do: send into the monitor loop.
struct Callbacks(Sender<Req>);

impl Callbacks {
    fn event(&self, e: Event) {
        let _ = self.0.send(Req::Event(e));
    }

    fn doubt(&self, reason: impl Into<String>) {
        self.event(Event::Doubt {
            reason: reason.into(),
            at: now_ns(),
        });
    }

    fn handoff(&self, msg: impl Any + Send) {
        let _ = self.0.send(Req::Handoff(Box::new(msg)));
    }
}

#[implement(IMMNotificationClient)]
struct Devices(Arc<Callbacks>);

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
struct NewSessions {
    cb: Arc<Callbacks>,
    device: String,
}

impl IAudioSessionNotification_Impl for NewSessions_Impl {
    fn OnSessionCreated(&self, session: Ref<IAudioSessionControl>) -> ComResult<()> {
        // Unknown until the owner has subscribed to it and a reconciliation agrees.
        self.cb.doubt("a new audio session is being subscribed");
        match session.ok() {
            Ok(c) => self.cb.handoff(Created {
                control: Mta(c.clone()),
                device: self.device.clone(),
            }),
            Err(e) => self
                .cb
                .doubt(format!("a new audio session was unusable: {e}")),
        }
        Ok(())
    }
}

#[implement(IAudioSessionEvents)]
struct SessionEvents {
    cb: Arc<Callbacks>,
    key: String,
    image: Option<String>,
}

impl SessionEvents {
    fn gone(&self) {
        self.cb.event(Event::Gone {
            key: self.key.clone(),
            at: now_ns(),
        });
        self.cb.handoff(Retire(self.key.clone()));
    }
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
        if state == AudioSessionStateExpired {
            self.gone();
        } else {
            self.cb.event(Event::Session {
                key: self.key.clone(),
                image: self.image.clone(),
                active: state == AudioSessionStateActive,
                at: now_ns(),
            });
        }
        Ok(())
    }
    fn OnSessionDisconnected(&self, _: AudioSessionDisconnectReason) -> ComResult<()> {
        self.gone();
        Ok(())
    }
}

struct Watched {
    control: IAudioSessionControl,
    events: IAudioSessionEvents,
    device: String,
}

/// Lives on the owner thread only; nothing here is shared with callbacks.
pub struct Wasapi {
    devices: IMMDeviceEnumerator,
    device_events: IMMNotificationClient,
    endpoints: HashMap<String, (IAudioSessionManager2, IAudioSessionNotification)>,
    sessions: HashMap<String, Watched>,
    /// Retired since the last snapshot.
    gone: Vec<String>,
    cb: Arc<Callbacks>,
}

impl Wasapi {
    /// Creates the device enumerator and registers for endpoint changes. The calling thread must be in the MTA.
    pub fn new(tx: Sender<Req>) -> Result<Wasapi, String> {
        let cb = Arc::new(Callbacks(tx));
        // SAFETY: COM calls on interfaces created here.
        unsafe {
            let devices: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| e.to_string())?;
            let device_events: IMMNotificationClient = Devices(cb.clone()).into();
            devices
                .RegisterEndpointNotificationCallback(&device_events)
                .map_err(|e| e.to_string())?;
            Ok(Wasapi {
                devices,
                device_events,
                endpoints: HashMap::new(),
                sessions: HashMap::new(),
                gone: vec![],
                cb,
            })
        }
    }

    /// Subscribes to the session's state changes (once), then reads it.
    fn subscribe(&mut self, control: IAudioSessionControl, device: &str) -> ComResult<Session> {
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
            if !self.sessions.contains_key(&key) {
                let events: IAudioSessionEvents = SessionEvents {
                    cb: self.cb.clone(),
                    key: key.clone(),
                    image: image.clone(),
                }
                .into();
                control.RegisterAudioSessionNotification(&events)?;
                self.sessions.insert(
                    key.clone(),
                    Watched {
                        control: control.clone(),
                        events,
                        device: device.to_string(),
                    },
                );
            }
            let active = control.GetState()? == AudioSessionStateActive;
            Ok(Session { key, image, active })
        }
    }

    fn retire(&mut self, key: &str) {
        if let Some(w) = self.sessions.remove(key) {
            // SAFETY: unregisters the callback registered in `subscribe`, on the owner thread.
            let _ = unsafe { w.control.UnregisterAudioSessionNotification(&w.events) };
            self.gone.push(key.to_string());
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
                    let created: IAudioSessionNotification = NewSessions {
                        cb: self.cb.clone(),
                        device: id.clone(),
                    }
                    .into();
                    // Before the first enumeration, so no session created meanwhile is missed.
                    manager.RegisterSessionNotification(&created)?;
                    self.endpoints.insert(id.clone(), (manager, created));
                }
                let sessions = self.endpoints[&id].0.GetSessionEnumerator()?;
                for j in 0..sessions.GetCount()? {
                    let s = self.subscribe(sessions.GetSession(j)?, &id)?;
                    snap.sessions.push(s);
                }
                snap.devices.push(id);
            }
        }
        // An endpoint that went away takes its sessions with it.
        let removed: Vec<String> = self
            .endpoints
            .keys()
            .filter(|id| !snap.devices.contains(id))
            .cloned()
            .collect();
        for id in removed {
            if let Some((manager, created)) = self.endpoints.remove(&id) {
                // SAFETY: unregisters the callback registered above.
                let _ = unsafe { manager.UnregisterSessionNotification(&created) };
            }
            let keys: Vec<String> = self
                .sessions
                .iter()
                .filter(|(_, w)| w.device == id)
                .map(|(k, _)| k.clone())
                .collect();
            for k in keys {
                self.retire(&k);
            }
        }
        snap.gone = std::mem::take(&mut self.gone);
        Ok(snap)
    }
}

impl Drop for Wasapi {
    fn drop(&mut self) {
        // SAFETY: unregisters every callback registered by this object, on the owner thread.
        unsafe {
            for w in self.sessions.values() {
                let _ = w.control.UnregisterAudioSessionNotification(&w.events);
            }
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

    fn handoff(&mut self, msg: Box<dyn Any + Send>, now: Ns) -> Vec<Event> {
        match msg.downcast::<Created>() {
            Ok(c) => {
                let Created { control, device } = *c;
                match self.subscribe(control.0, &device) {
                    Ok(s) => vec![Event::Session {
                        key: s.key,
                        image: s.image,
                        active: s.active,
                        at: now,
                    }],
                    Err(e) => vec![Event::Doubt {
                        reason: format!("a new audio session couldn't be subscribed: {e}"),
                        at: now,
                    }],
                }
            }
            Err(msg) => {
                if let Ok(r) = msg.downcast::<Retire>() {
                    self.retire(&r.0);
                }
                vec![]
            }
        }
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

    fn handoff(&mut self, msg: Box<dyn Any + Send>, now: Ns) -> Vec<Event> {
        match self.inner.as_mut() {
            Some(w) => w.handoff(msg, now),
            // From a connection that is gone: the next snapshot starts over.
            None => vec![],
        }
    }
}
