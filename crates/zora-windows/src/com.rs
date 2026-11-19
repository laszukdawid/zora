//! A COM apartment for the current thread, entered once and left when dropped (after every interface the thread
//! used has been released: declare it before them).

use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize,
};

pub struct Apartment(std::marker::PhantomData<*const ()>);

impl Apartment {
    /// The multithreaded apartment (the dictation monitor) or a single-threaded one (the tray's UI thread).
    /// S_OK and S_FALSE both count once and are balanced on drop; any failure (such as a thread already in the
    /// other kind of apartment) is an error.
    pub fn enter(multithreaded: bool) -> Result<Apartment, String> {
        let kind = if multithreaded {
            COINIT_MULTITHREADED
        } else {
            COINIT_APARTMENTTHREADED
        };
        // SAFETY: plain COM setup on the calling thread.
        let hr = unsafe { CoInitializeEx(None, kind) };
        if hr.is_err() {
            return Err(format!("cannot enter a COM apartment: {hr}"));
        }
        Ok(Apartment(std::marker::PhantomData))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx of this thread.
        unsafe { CoUninitialize() };
    }
}
