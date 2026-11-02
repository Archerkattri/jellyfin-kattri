//! System-wide playback shortcuts.
//!
//! Only Windows registers keys with the OS today: `RegisterHotKey` plus a message loop on a
//! dedicated thread. Other platforms compile the no-op install and keep their shortcuts
//! application-focused. Registration failures are logged and leave the app running normally.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GlobalAction {
    TogglePause,
    SeekBackward,
    SeekForward,
    ToggleMute,
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::ptr::null_mut;

    use winit::event_loop::EventLoopProxy;

    use crate::HostEvent;

    use super::GlobalAction;

    const MOD_ALT: u32 = 0x0001;
    const MOD_CONTROL: u32 = 0x0002;
    const MOD_NOREPEAT: u32 = 0x4000;
    const WM_HOTKEY: u32 = 0x0312;
    const VK_SPACE: u32 = 0x20;
    const VK_LEFT: u32 = 0x25;
    const VK_RIGHT: u32 = 0x27;
    const VK_M: u32 = 0x4D;

    #[repr(C)]
    #[derive(Default)]
    struct Msg {
        hwnd: *mut c_void,
        message: u32,
        w_param: usize,
        l_param: isize,
        time: u32,
        pt_x: i32,
        pt_y: i32,
        private: u32,
    }

    unsafe extern "system" {
        fn RegisterHotKey(hwnd: *mut c_void, id: i32, modifiers: u32, vk: u32) -> i32;
        fn UnregisterHotKey(hwnd: *mut c_void, id: i32) -> i32;
        fn GetMessageW(msg: *mut Msg, hwnd: *mut c_void, min: u32, max: u32) -> i32;
    }

    pub(crate) fn install(proxy: EventLoopProxy<HostEvent>, enabled: bool) {
        if !enabled {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("kattri-global-shortcuts".to_owned())
            .spawn(move || {
                let bindings = [
                    (1, VK_SPACE, GlobalAction::TogglePause),
                    (2, VK_LEFT, GlobalAction::SeekBackward),
                    (3, VK_RIGHT, GlobalAction::SeekForward),
                    (4, VK_M, GlobalAction::ToggleMute),
                ];
                let mut registered = 0;
                // SAFETY: no pointers are stored by the API; the thread owns the registrations.
                unsafe {
                    for (id, key, _) in bindings {
                        if RegisterHotKey(null_mut(), id, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, key)
                            == 0
                        {
                            eprintln!("global shortcuts: key {key:#x} is already reserved");
                            for (done, _, _) in bindings.iter().take(registered) {
                                UnregisterHotKey(null_mut(), *done);
                            }
                            return;
                        }
                        registered += 1;
                    }

                    let mut message = Msg::default();
                    loop {
                        let result = GetMessageW(&mut message, null_mut(), 0, 0);
                        if result <= 0 {
                            break;
                        }
                        if message.message != WM_HOTKEY {
                            continue;
                        }
                        let action = bindings
                            .iter()
                            .find(|(id, _, _)| *id as usize == message.w_param)
                            .map(|(_, _, action)| *action);
                        if let Some(action) = action
                            && proxy.send_event(HostEvent::GlobalShortcut(action)).is_err()
                        {
                            break;
                        }
                    }
                }
            });
        if spawned.is_err() {
            eprintln!("global shortcuts: could not start the hotkey thread");
        }
    }
}

pub(crate) use imp::install;
