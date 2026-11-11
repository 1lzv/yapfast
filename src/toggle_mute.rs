//! The toggle-mute key: one press mutes the microphone, the next unmutes it,
//! in any app, whatever the input mode, so voice activity stays on. On
//! Windows a low-level keyboard or mouse hook sees the key, or Mouse 4 or 5,
//! and passes every event on untouched; where no hook works, the key works
//! while Yapfast's window has the focus, and settings say so.
//!
//! The key only says "toggle"; muting itself is the call's own mute, which
//! reaches Discord and the other people on the call.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering},
};

#[cfg(windows)]
use slint::winit_030::winit::event::MouseButton;
use slint::winit_030::winit::{
    event::{ElementState, WindowEvent},
    platform::scancode::PhysicalKeyExtScancode,
};

use crate::push_to_talk::{Keybind, Reach};
#[cfg(windows)]
use crate::push_to_talk::{MOUSE_4, MOUSE_5};

const NONE: u32 = u32::MAX;

/// The key's scan code while the window's own key events stand in for a
/// global key; [`NONE`] otherwise.
static FOCUSED_KEY: AtomicU32 = AtomicU32::new(NONE);
/// The key's scan code for as long as the toggle-mute key is watched, in any
/// app or only in the window; [`NONE`] otherwise.
static WATCHED_KEY: AtomicU32 = AtomicU32::new(NONE);

/// Tells one press of a key apart from the repeats a held key sends: it
/// counts a press only when the key wasn't already down.
#[cfg(any(windows, test))]
struct Edge(std::sync::atomic::AtomicBool);

#[cfg(any(windows, test))]
impl Edge {
    const fn new() -> Self {
        Self(std::sync::atomic::AtomicBool::new(false))
    }

    /// The key went down; whether that's a new press.
    fn down(&self) -> bool {
        !self.0.swap(true, Ordering::Relaxed)
    }

    /// The key came up.
    fn up(&self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// Whether this window event, handled on the interface's thread, is the press
/// that toggles mute while no global key works. Repeats of a held key don't
/// count.
pub fn window_event(event: &WindowEvent) -> bool {
    let key = FOCUSED_KEY.load(Ordering::Relaxed);
    if key == NONE {
        return false;
    }
    match event {
        WindowEvent::KeyboardInput { event, .. } => {
            event.state == ElementState::Pressed
                && !event.repeat
                && event.physical_key.to_scancode() == Some(key)
        }
        #[cfg(windows)]
        WindowEvent::MouseInput {
            state: ElementState::Pressed,
            button,
            ..
        } => {
            (key == MOUSE_4 && *button == MouseButton::Back)
                || (key == MOUSE_5 && *button == MouseButton::Forward)
        }
        _ => false,
    }
}

/// Whether this mouse button is the toggle-mute key, which then isn't also the
/// window's Back or Forward.
pub fn takes(button: slint::winit_030::winit::event::MouseButton) -> bool {
    #[cfg(windows)]
    {
        let key = WATCHED_KEY.load(Ordering::Relaxed);
        (key == MOUSE_4 && button == MouseButton::Back)
            || (key == MOUSE_5 && button == MouseButton::Forward)
    }
    #[cfg(not(windows))]
    {
        let _ = button;
        false
    }
}

/// What settings say under the key.
pub fn note(reach: &Reach) -> String {
    match reach {
        Reach::Asking | Reach::Everywhere(_) => {
            "Press the key in any app to mute or unmute your microphone. Your input mode stays as it is.".into()
        }
        Reach::Focused(why) => format!(
            "{why}, so the toggle mute key works only while Yapfast's window is in front."
        ),
    }
}

/// Watches the key in every app it can, until dropped, and calls `toggle` once
/// for each press.
pub struct Watcher {
    key: Keybind,
    reach: Arc<Mutex<Reach>>,
    #[cfg(windows)]
    _global: Option<windows::Hook>,
}

impl Watcher {
    /// Starts watching `key`; `told` hears where it works whenever that
    /// changes, and `toggle` each press, both from any thread.
    pub fn start(
        key: Keybind,
        told: impl Fn(Reach) + Send + Sync + 'static,
        toggle: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let reach = Arc::new(Mutex::new(Reach::Asking));
        let shared = reach.clone();
        let scancode = key.scancode;
        WATCHED_KEY.store(scancode, Ordering::Relaxed);
        let report = Arc::new(move |now: Reach| {
            let focused = matches!(now, Reach::Focused(_));
            FOCUSED_KEY.store(if focused { scancode } else { NONE }, Ordering::Relaxed);
            *shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = now.clone();
            told(now);
        });
        #[cfg(windows)]
        let global = windows::Hook::start(scancode, Arc::new(toggle), report);
        #[cfg(not(windows))]
        {
            let _ = toggle;
            report(Reach::Focused(
                "Yapfast can't watch keys in other apps on this system yet",
            ));
        }
        Self {
            key,
            reach,
            #[cfg(windows)]
            _global: global,
        }
    }

    pub fn key(&self) -> &Keybind {
        &self.key
    }

    pub fn reach(&self) -> Reach {
        self.reach
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        FOCUSED_KEY.store(NONE, Ordering::Relaxed);
        WATCHED_KEY.store(NONE, Ordering::Relaxed);
    }
}

#[cfg(windows)]
type Report = Arc<dyn Fn(Reach) + Send + Sync>;

#[cfg(windows)]
mod windows {
    use std::{
        ptr,
        sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
            mpsc,
        },
    };

    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, HANDLE, LPARAM, LRESULT, WPARAM},
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        System::{
            LibraryLoader::GetModuleHandleW,
            Threading::{
                GetCurrentProcess, GetCurrentThread, GetCurrentThreadId, OpenProcessToken,
                SetThreadPriority, THREAD_PRIORITY_HIGHEST,
            },
        },
        UI::WindowsAndMessaging::{
            CallNextHookEx, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG,
            MSLLHOOKSTRUCT, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, SetWindowsHookExW,
            UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_KEYDOWN, WM_KEYUP,
            WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, XBUTTON1, XBUTTON2,
        },
    };

    use super::{Edge, MOUSE_4, MOUSE_5, NONE, Report};

    /// The key or tagged mouse button the hook watches.
    static KEY: AtomicU32 = AtomicU32::new(NONE);
    /// The thread the hook runs on, which its procedures wake with a press;
    /// 0 while there is none.
    static THREAD: AtomicU32 = AtomicU32::new(0);
    /// Whether the watched keyboard key is down, so its repeats count once.
    static DOWN: Edge = Edge::new();

    /// Whether Yapfast runs as administrator. Windows hands a hook no input
    /// aimed at a window of a higher privilege, such as a game started as
    /// administrator, so the log says which side Yapfast is on.
    fn elevated() -> Option<bool> {
        // SAFETY: the token handle is ours until closed, and the buffer is the
        // size the call is told.
        unsafe {
            let mut token: HANDLE = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut returned = 0u32;
            let read = GetTokenInformation(
                token,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            );
            CloseHandle(token);
            (read != 0).then_some(elevation.TokenIsElevated != 0)
        }
    }

    /// A low-level keyboard or mouse hook on a thread of its own, which sees
    /// every input event and passes each on. A press only wakes that thread's
    /// message loop, which calls the toggle outside the hook procedure: Windows
    /// drops a hook that is slow to answer. Removed when dropped.
    pub struct Hook {
        thread: u32,
    }

    impl Hook {
        pub fn start(
            scancode: u32,
            toggle: Arc<dyn Fn() + Send + Sync>,
            report: Report,
        ) -> Option<Self> {
            KEY.store(scancode, Ordering::Relaxed);
            DOWN.up();
            let mouse = matches!(scancode, MOUSE_4 | MOUSE_5);
            tracing::info!(
                key = scancode,
                mouse,
                elevated = ?elevated(),
                "starting the toggle-mute key hook"
            );
            let (started, thread) = mpsc::sync_channel(1);
            let spawned = std::thread::Builder::new()
                .name("yapfast-toggle-mute-key".into())
                .spawn(move || {
                    // SAFETY: the hook procedures below only read the event
                    // Windows hands them; the message loop runs on this
                    // thread, which the hook needs, until WM_QUIT.
                    unsafe {
                        let hook = if mouse {
                            SetWindowsHookExW(
                                WH_MOUSE_LL,
                                Some(mouse_proc),
                                GetModuleHandleW(ptr::null()),
                                0,
                            )
                        } else {
                            SetWindowsHookExW(
                                WH_KEYBOARD_LL,
                                Some(proc),
                                GetModuleHandleW(ptr::null()),
                                0,
                            )
                        };
                        if hook.is_null() {
                            tracing::warn!(
                                error = GetLastError(),
                                "Windows didn't install the toggle-mute key hook"
                            );
                            let _ = started.send(None);
                            return;
                        }
                        // The hook answers from this thread: a starved thread
                        // answers late, and Windows drops a hook that does.
                        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
                        let id = GetCurrentThreadId();
                        let mut message: MSG = std::mem::zeroed();
                        // Gives the thread its message queue before anyone
                        // posts to it.
                        PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_NOREMOVE);
                        THREAD.store(id, Ordering::Relaxed);
                        let _ = started.send(Some(id));
                        tracing::info!("the toggle-mute key hook is installed");
                        while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
                            if message.message == WM_APP {
                                // Logged here, not in the hook procedure, which
                                // must answer at once. A press that never shows
                                // in the log never reached the hook.
                                tracing::info!("the toggle-mute key was pressed");
                                toggle();
                            }
                        }
                        // Only this hook's own id: a newer hook, started
                        // while this one ended, has its own.
                        let _ =
                            THREAD.compare_exchange(id, 0, Ordering::Relaxed, Ordering::Relaxed);
                        UnhookWindowsHookEx(hook);
                        tracing::info!("the toggle-mute key hook is removed");
                    }
                });
            let thread = spawned.ok().and_then(|_| thread.recv().ok().flatten());
            match thread {
                Some(thread) => {
                    report(super::Reach::Everywhere(None));
                    Some(Self { thread })
                }
                None => {
                    report(super::Reach::Focused(
                        "Windows didn't let Yapfast watch keys in other apps",
                    ));
                    None
                }
            }
        }
    }

    impl Drop for Hook {
        fn drop(&mut self) {
            KEY.store(NONE, Ordering::Relaxed);
            // SAFETY: posting to a thread id is harmless if it has ended.
            unsafe {
                PostThreadMessageW(self.thread, WM_QUIT, 0, 0);
            }
        }
    }

    /// Asks the hook's thread to toggle mute.
    fn wake() {
        let thread = THREAD.load(Ordering::Relaxed);
        if thread != 0 {
            // SAFETY: posting to a thread id is harmless if it has ended.
            unsafe {
                PostThreadMessageW(thread, WM_APP, 0, 0);
            }
        }
    }

    /// The side button a mouse event's data names: XBUTTON1 is Mouse 4 (Back),
    /// XBUTTON2 is Mouse 5 (Forward), in the data's high word.
    fn side_button(data: u32) -> u32 {
        match (data >> 16) as u16 {
            XBUTTON1 => MOUSE_4,
            XBUTTON2 => MOUSE_5,
            _ => NONE,
        }
    }

    /// Whether a mouse event is the press of the watched side button. Its
    /// release, and every other button, don't count.
    fn mouse_toggles(message: u32, data: u32, key: u32) -> bool {
        key != NONE && message == WM_XBUTTONDOWN && side_button(data) == key
    }

    /// Whether a keyboard event is a new press of the watched key: a held
    /// key's repeats, and its release, don't count.
    fn key_toggles(message: u32, scan_code: u32, flags: u32, key: u32, held: &Edge) -> bool {
        let extended = if flags & LLKHF_EXTENDED != 0 {
            0xE000
        } else {
            0
        };
        if key == NONE || scan_code | extended != key {
            return false;
        }
        match message {
            WM_KEYDOWN | WM_SYSKEYDOWN => held.down(),
            WM_KEYUP | WM_SYSKEYUP => {
                held.up();
                false
            }
            _ => false,
        }
    }

    /// Notes a press of the toggle-mute key, and passes every key on.
    unsafe extern "system" fn proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            // SAFETY: for HC_ACTION, lparam points at the key's details.
            let key = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            if key_toggles(
                wparam as u32,
                key.scanCode,
                key.flags,
                KEY.load(Ordering::Relaxed),
                &DOWN,
            ) {
                wake();
            }
        }
        // SAFETY: the next hook gets the event as it came.
        unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
    }

    /// Notes a press of the toggle-mute side button, and passes every mouse
    /// event on.
    unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            // SAFETY: for HC_ACTION, lparam points at the mouse event's details.
            let mouse = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
            if mouse_toggles(wparam as u32, mouse.mouseData, KEY.load(Ordering::Relaxed)) {
                wake();
            }
        }
        // SAFETY: the next hook gets the event as it came.
        unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
    }

    #[cfg(test)]
    mod tests {
        use std::{
            sync::atomic::AtomicUsize,
            time::{Duration, Instant},
        };

        use windows_sys::Win32::UI::{
            Input::KeyboardAndMouse::{
                INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
                SendInput,
            },
            WindowsAndMessaging::{WM_XBUTTONUP, XBUTTON1},
        };

        use super::*;

        /// The hook tests share the watched key and the hook's thread.
        static HOOK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        fn data(button: u16) -> u32 {
            u32::from(button) << 16
        }

        #[test]
        fn side_buttons_are_mouse_4_and_5() {
            assert_eq!(side_button(data(XBUTTON1)), MOUSE_4);
            assert_eq!(side_button(data(XBUTTON2)), MOUSE_5);
            assert_eq!(side_button(data(3)), NONE);
            assert_eq!(side_button(0), NONE);
        }

        #[test]
        fn a_side_button_toggles_once_on_its_press() {
            for (button, key, other) in [(XBUTTON1, MOUSE_4, MOUSE_5), (XBUTTON2, MOUSE_5, MOUSE_4)]
            {
                assert!(mouse_toggles(WM_XBUTTONDOWN, data(button), key));
                assert!(
                    !mouse_toggles(WM_XBUTTONUP, data(button), key),
                    "its release shouldn't toggle"
                );
                assert!(
                    !mouse_toggles(WM_XBUTTONDOWN, data(button), other),
                    "the other side button shouldn't toggle"
                );
                assert!(!mouse_toggles(WM_XBUTTONDOWN, data(button), NONE));
            }
            // Nothing is watched, and a click of another button comes through.
            assert!(!mouse_toggles(WM_XBUTTONDOWN, 0, NONE));
        }

        #[test]
        fn a_held_key_toggles_once() {
            let held = Edge::new();
            let (key, scan_code) = (0x2F, 0x2F);
            let press = |message| key_toggles(message, scan_code, 0, key, &held);
            assert!(press(WM_KEYDOWN), "the first press toggles");
            // Holding the key sends repeats of the press.
            assert!(!press(WM_KEYDOWN));
            assert!(!press(WM_KEYDOWN));
            assert!(!press(WM_KEYUP));
            assert!(press(WM_KEYDOWN), "pressing it again toggles again");
            assert!(!key_toggles(WM_KEYDOWN, 0x30, 0, key, &held));
        }

        #[test]
        fn an_extended_key_is_not_its_plain_twin() {
            let held = Edge::new();
            // Right Ctrl is 0xE01D; Left Ctrl, 0x1D, is not it.
            assert!(!key_toggles(WM_KEYDOWN, 0x1D, 0, 0xE01D, &held));
            assert!(key_toggles(WM_KEYDOWN, 0x1D, LLKHF_EXTENDED, 0xE01D, &held));
        }

        /// Clicks a side button, as a mouse would.
        fn send_mouse(button: u16, up: bool) {
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: u32::from(button),
                        dwFlags: if up {
                            MOUSEEVENTF_XUP
                        } else {
                            MOUSEEVENTF_XDOWN
                        },
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            // SAFETY: one input, of the size Windows expects.
            let sent = unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) };
            assert_eq!(sent, 1);
        }

        /// Needs a Windows desktop that takes injected input.
        #[test]
        #[ignore = "sends real mouse input"]
        fn mouse_4_toggles_once_per_click_through_the_real_hook() {
            let _hook = HOOK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let count = Arc::new(AtomicUsize::new(0));
            let counted = count.clone();
            let hook = Hook::start(
                MOUSE_4,
                Arc::new(move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                }),
                Arc::new(|_| {}),
            )
            .expect("the hook should start on a desktop");
            let wait_for = |wanted: usize| {
                let until = Instant::now() + Duration::from_secs(2);
                while count.load(Ordering::SeqCst) != wanted {
                    assert!(Instant::now() < until, "expected {wanted} toggles");
                    std::thread::sleep(Duration::from_millis(10));
                }
            };
            send_mouse(XBUTTON1, false);
            send_mouse(XBUTTON1, true);
            wait_for(1);
            send_mouse(XBUTTON1, false);
            send_mouse(XBUTTON1, true);
            wait_for(2);
            // The other side button isn't it.
            send_mouse(XBUTTON2, false);
            send_mouse(XBUTTON2, true);
            std::thread::sleep(Duration::from_millis(200));
            assert_eq!(count.load(Ordering::SeqCst), 2);
            drop(hook);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_key_is_one_press() {
        let edge = Edge::new();
        assert!(edge.down());
        assert!(!edge.down(), "a repeat isn't a new press");
        assert!(!edge.down());
        edge.up();
        assert!(edge.down(), "the next press is");
    }

    #[test]
    fn settings_say_where_the_key_works() {
        assert!(note(&Reach::Everywhere(None)).contains("any app"));
        assert!(
            note(&Reach::Focused("This system can't"))
                .contains("only while Yapfast's window is in front")
        );
    }
}
