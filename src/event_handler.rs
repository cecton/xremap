use crate::client::{build_client, WMClient};
use crate::config::action::Action;
use crate::config::application::Application;
use crate::config::key_action::{KeyAction, ModifierKey, MultiPurposeKey, PressReleaseKey};
use crate::config::key_press::{KeyPress, Modifier};
use crate::config::keymap::{build_override_table, OverrideEntry};
use crate::config::remap::Remap;
use crate::Config;
use evdev::uinput::VirtualDevice;
use evdev::{EventType, InputEvent, Key};
use lazy_static::lazy_static;
use log::{debug, error};
use nix::sys::signal;
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet};
use nix::sys::time::TimeSpec;
use nix::sys::timerfd::{Expiration, TimerFd, TimerSetTimeFlags};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::process::{Command, Stdio};
use std::time::Instant;

pub struct EventHandler {
    // Device to emit events
    device: VirtualDevice,
    // Currently pressed modifier keys
    modifiers: HashSet<Key>,
    // Make sure the original event is released even if remapping changes while holding the key
    pressed_keys: HashMap<Key, Key>,
    // Check the currently active application
    application_client: WMClient,
    application_cache: Option<String>,
    // State machine for multi-purpose keys
    multi_purpose_keys: HashMap<Key, MultiPurposeKeyState>,
    // Current nested remaps
    override_remap: Option<HashMap<Key, Vec<OverrideEntry>>>,
    // Key triggered on a timeout of nested remaps
    override_timeout_key: Option<Key>,
    // Trigger a timeout of nested remaps through select(2)
    override_timer: TimerFd,
    // Whether we've called a sigaction for spawing commands or not
    sigaction_set: bool,
    // { set_mode: String }
    mode: String,
    // { set_mark: true }
    mark_set: bool,
    // { escape_next_key: true }
    escape_next_key: bool,
}

impl EventHandler {
    pub fn new(device: VirtualDevice, timer: TimerFd, mode: &str) -> EventHandler {
        EventHandler {
            device,
            modifiers: HashSet::new(),
            pressed_keys: HashMap::new(),
            application_client: build_client(),
            application_cache: None,
            multi_purpose_keys: HashMap::new(),
            override_remap: None,
            override_timeout_key: None,
            override_timer: timer,
            sigaction_set: false,
            mode: mode.to_string(),
            mark_set: false,
            escape_next_key: false,
        }
    }

    // Handle EventType::KEY
    pub fn on_event(&mut self, event: InputEvent, config: &Config) -> Result<(), Box<dyn Error>> {
        self.application_cache = None; // expire cache
        let key = Key::new(event.code());
        debug!("=> {}: {:?}", event.value(), &key);

        // Apply modmap
        let mut key_values = if let Some(key_action) = self.find_modmap(config, &key) {
            self.dispatch_keys(key_action, key, event.value())?
        } else {
            vec![(key, event.value())]
        };
        self.maintain_pressed_keys(key, event.value(), &mut key_values);
        if !self.multi_purpose_keys.is_empty() {
            key_values = self.flush_timeout_keys(key_values);
        }

        // Apply keymap
        for (key, value) in key_values.into_iter() {
            if MODIFIER_KEYS.contains(&key.code()) {
                self.update_modifier(key, value);
            } else if is_pressed(value) {
                if self.escape_next_key {
                    self.escape_next_key = false
                } else if let Some(actions) = self.find_keymap(config, &key)? {
                    self.dispatch_actions(&actions, &key)?;
                    continue;
                }
            }
            self.send_key(&key, value)?;
        }
        Ok(())
    }

    pub fn send_event(&mut self, event: InputEvent) -> std::io::Result<()> {
        if event.event_type() == EventType::KEY {
            debug!("{}: {:?}", event.value(), Key::new(event.code()))
        }
        self.device.emit(&[event])
    }

    pub fn timeout_override(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(key) = self.override_timeout_key {
            self.send_key(&key, PRESS)?;
            self.send_key(&key, RELEASE)?;
        }
        self.remove_override()
    }

    fn remove_override(&mut self) -> Result<(), Box<dyn Error>> {
        self.override_timer.unset()?;
        self.override_remap = None;
        self.override_timeout_key = None;
        Ok(())
    }

    fn send_keys(&mut self, keys: &Vec<Key>, value: i32) -> std::io::Result<()> {
        for key in keys {
            self.send_key(key, value)?;
        }
        Ok(())
    }

    fn send_key(&mut self, key: &Key, value: i32) -> std::io::Result<()> {
        let event = InputEvent::new(EventType::KEY, key.code(), value);
        self.send_event(event)
    }

    // Repeat/Release what's originally pressed even if remapping changes while holding it
    fn maintain_pressed_keys(&mut self, key: Key, value: i32, events: &mut Vec<(Key, i32)>) {
        // Not handling multi-purpose keysfor now; too complicated
        if events.len() != 1 || value != events[0].1 {
            return;
        }

        let event = events[0];
        if value == PRESS {
            self.pressed_keys.insert(key, event.0);
        } else {
            if let Some(original_key) = self.pressed_keys.get(&key) {
                events[0].0 = *original_key;
            }
            if value == RELEASE {
                self.pressed_keys.remove(&key);
            }
        }
    }

    fn dispatch_keys(
        &mut self,
        key_action: KeyAction,
        key: Key,
        value: i32,
    ) -> Result<Vec<(Key, i32)>, Box<dyn Error>> {
        let keys = match key_action {
            KeyAction::Key(modmap_key) => vec![(modmap_key, value)],
            KeyAction::ModifierKey(ModifierKey { modifier }) => {
                if modifier {
                    if value == PRESS {
                        self.modifiers.insert(key);
                    } else if value == RELEASE {
                        self.modifiers.remove(&key);
                    }
                }
                // Process this key only in xremap
                vec![]
            }
            KeyAction::MultiPurposeKey(MultiPurposeKey {
                held,
                alone,
                alone_timeout,
            }) => {
                if value == PRESS {
                    self.multi_purpose_keys.insert(
                        key,
                        MultiPurposeKeyState {
                            held,
                            alone,
                            alone_timeout_at: Some(Instant::now() + alone_timeout),
                        },
                    );
                    return Ok(vec![]); // delay the press
                } else if value == REPEAT {
                    if let Some(state) = self.multi_purpose_keys.get_mut(&key) {
                        return Ok(state.repeat());
                    }
                } else if value == RELEASE {
                    if let Some(state) = self.multi_purpose_keys.remove(&key) {
                        return Ok(state.release());
                    }
                } else {
                    panic!("unexpected key event value: {}", value);
                }
                // fallthrough on state discrepancy
                vec![(key, value)]
            }
            KeyAction::PressReleaseKey(PressReleaseKey { press, release }) => {
                if value == PRESS {
                    self.dispatch_actions(&press, &key)?;
                }
                if value == RELEASE {
                    self.dispatch_actions(&release, &key)?;
                }
                // While triggered by modmap, PressReleaseKey is not remapped by keymap.
                vec![]
            }
        };
        Ok(keys)
    }

    fn flush_timeout_keys(&mut self, key_values: Vec<(Key, i32)>) -> Vec<(Key, i32)> {
        let mut flush = false;
        for (_, value) in key_values.iter() {
            if *value == PRESS {
                flush = true;
                break;
            }
        }

        if flush {
            let mut flushed: Vec<(Key, i32)> = vec![];
            for (_, state) in self.multi_purpose_keys.iter_mut() {
                flushed.extend(state.force_held());
            }
            flushed.extend(key_values);
            flushed
        } else {
            key_values
        }
    }

    fn find_modmap(&mut self, config: &Config, key: &Key) -> Option<KeyAction> {
        for modmap in &config.modmap {
            if let Some(key_action) = modmap.remap.get(key) {
                if let Some(application_matcher) = &modmap.application {
                    if !self.match_application(application_matcher) {
                        continue;
                    }
                }
                return Some(key_action.clone());
            }
        }
        None
    }

    fn find_keymap(&mut self, config: &Config, key: &Key) -> Result<Option<Vec<Action>>, Box<dyn Error>> {
        if let Some(override_remap) = &self.override_remap {
            if let Some(entries) = override_remap.get(key) {
                for exact_match in [true, false] {
                    for entry in entries {
                        if !self.match_modifiers(&entry.modifiers, exact_match) {
                            continue;
                        }
                        return Ok(Some(entry.actions.to_vec()));
                    }
                }
            }
        }
        if let Some(entries) = config.keymap_table.get(key) {
            for exact_match in [true, false] {
                for entry in entries {
                    if !self.match_modifiers(&entry.modifiers, exact_match) {
                        continue;
                    }
                    if let Some(application_matcher) = &entry.application {
                        if !self.match_application(application_matcher) {
                            continue;
                        }
                    }
                    if let Some(modes) = &entry.mode {
                        if !modes.contains(&self.mode) {
                            continue;
                        }
                    }
                    return Ok(Some(entry.actions.clone()));
                }
            }
        }
        Ok(None)
    }

    fn dispatch_actions(&mut self, actions: &Vec<Action>, key: &Key) -> Result<(), Box<dyn Error>> {
        for action in actions {
            self.dispatch_action(action, key)?;
        }
        Ok(())
    }

    fn dispatch_action(&mut self, action: &Action, key: &Key) -> Result<(), Box<dyn Error>> {
        match action {
            Action::KeyPress(key_press) => self.send_key_press(key_press)?,
            Action::Remap(Remap {
                remap,
                timeout,
                timeout_key,
            }) => {
                self.override_remap = Some(build_override_table(remap));
                if let Some(timeout) = timeout {
                    let expiration = Expiration::OneShot(TimeSpec::from_duration(*timeout));
                    self.override_timer.unset()?;
                    self.override_timer.set(expiration, TimerSetTimeFlags::empty())?;
                    self.override_timeout_key = timeout_key.or_else(|| Some(*key));
                }
            }
            Action::Launch(command) => self.run_command(command.clone()),
            Action::SetMode(mode) => {
                self.mode = mode.clone();
                println!("mode: {}", mode);
            }
            Action::SetMark(set) => self.mark_set = *set,
            Action::WithMark(key_press) => self.send_key_press(&self.with_mark(key_press))?,
            Action::EscapeNextKey(escape_next_key) => self.escape_next_key = *escape_next_key,
        }
        Ok(())
    }

    fn send_key_press(&mut self, key_press: &KeyPress) -> Result<(), Box<dyn Error>> {
        // Build missing or extra modifiers
        let extra_modifiers: Vec<Key> = self
            .modifiers
            .iter()
            .filter_map(|modifier| {
                if self.contains_modifier(&key_press.modifiers, modifier) {
                    None
                } else {
                    Some(modifier.clone())
                }
            })
            .collect();
        let missing_modifiers: Vec<Key> = key_press
            .modifiers
            .iter()
            .filter_map(|modifier| {
                if self.match_modifier(modifier) {
                    None
                } else {
                    match modifier {
                        Modifier::Shift => Some(Key::KEY_LEFTSHIFT),
                        Modifier::Control => Some(Key::KEY_LEFTCTRL),
                        Modifier::Alt => Some(Key::KEY_LEFTALT),
                        Modifier::Windows => Some(Key::KEY_LEFTMETA),
                        Modifier::Key(key) => Some(key.clone()),
                    }
                }
            })
            .collect();

        // Emulate the modifiers of KeyPress
        self.send_keys(&extra_modifiers, RELEASE)?;
        self.send_keys(&missing_modifiers, PRESS)?;

        // Press the main key
        self.send_key(&key_press.key, PRESS)?;
        self.send_key(&key_press.key, RELEASE)?;

        // Resurrect the original modifiers
        self.send_keys(&missing_modifiers, RELEASE)?;
        self.send_keys(&extra_modifiers, PRESS)?;

        Ok(())
    }

    fn with_mark(&self, key_press: &KeyPress) -> KeyPress {
        if self.mark_set && !self.match_modifier(&Modifier::Shift) {
            let mut modifiers = key_press.modifiers.clone();
            modifiers.push(Modifier::Shift);
            KeyPress {
                key: key_press.key,
                modifiers,
            }
        } else {
            key_press.clone()
        }
    }

    fn run_command(&mut self, command: Vec<String>) {
        if !self.sigaction_set {
            // Avoid defunct processes
            let sig_action = SigAction::new(SigHandler::SigDfl, SaFlags::SA_NOCLDWAIT, SigSet::empty());
            unsafe {
                sigaction(signal::SIGCHLD, &sig_action).expect("Failed to register SIGCHLD handler");
            }
            self.sigaction_set = true;
        }

        debug!("Running command: {:?}", command);
        match Command::new(&command[0])
            .args(&command[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => debug!("Process spawned: {:?}, pid {}", command, child.id()),
            Err(e) => error!("Error running command: {:?}", e),
        }
    }

    fn contains_modifier(&self, modifiers: &Vec<Modifier>, key: &Key) -> bool {
        for modifier in modifiers {
            if match modifier {
                Modifier::Shift => key == &Key::KEY_LEFTSHIFT || key == &Key::KEY_RIGHTSHIFT,
                Modifier::Control => key == &Key::KEY_LEFTCTRL || key == &Key::KEY_RIGHTCTRL,
                Modifier::Alt => key == &Key::KEY_LEFTALT || key == &Key::KEY_RIGHTALT,
                Modifier::Windows => key == &Key::KEY_LEFTMETA || key == &Key::KEY_RIGHTMETA,
                Modifier::Key(modifier_key) => key == modifier_key,
            } {
                return true;
            }
        }
        false
    }

    fn match_modifiers(&self, modifiers: &Vec<Modifier>, exact_match: bool) -> bool {
        if exact_match && self.modifiers.len() > modifiers.len() {
            return false;
        }
        for modifier in modifiers {
            if !self.match_modifier(modifier) {
                return false;
            }
        }
        return true;
    }

    fn match_modifier(&self, modifier: &Modifier) -> bool {
        match modifier {
            Modifier::Shift => {
                self.modifiers.contains(&Key::KEY_LEFTSHIFT) || self.modifiers.contains(&Key::KEY_RIGHTSHIFT)
            }
            Modifier::Control => {
                self.modifiers.contains(&Key::KEY_LEFTCTRL) || self.modifiers.contains(&Key::KEY_RIGHTCTRL)
            }
            Modifier::Alt => self.modifiers.contains(&Key::KEY_LEFTALT) || self.modifiers.contains(&Key::KEY_RIGHTALT),
            Modifier::Windows => {
                self.modifiers.contains(&Key::KEY_LEFTMETA) || self.modifiers.contains(&Key::KEY_RIGHTMETA)
            }
            Modifier::Key(key) => self.modifiers.contains(key),
        }
    }

    fn match_application(&mut self, application_matcher: &Application) -> bool {
        // Lazily fill the wm_class cache
        if self.application_cache.is_none() {
            match self.application_client.current_application() {
                Some(application) => self.application_cache = Some(application),
                None => self.application_cache = Some(String::new()),
            }
        }

        if let Some(application) = &self.application_cache {
            if let Some(application_only) = &application_matcher.only {
                return application_only.iter().any(|m| m.matches(application));
            }
            if let Some(application_not) = &application_matcher.not {
                return application_not.iter().all(|m| !m.matches(application));
            }
        }
        false
    }

    fn update_modifier(&mut self, key: Key, value: i32) {
        if value == PRESS {
            self.modifiers.insert(key);
        } else if value == RELEASE {
            self.modifiers.remove(&key);
        }
    }
}

lazy_static! {
    static ref MODIFIER_KEYS: [u16; 8] = [
        // Shift
        Key::KEY_LEFTSHIFT.code(),
        Key::KEY_RIGHTSHIFT.code(),
        // Control
        Key::KEY_LEFTCTRL.code(),
        Key::KEY_RIGHTCTRL.code(),
        // Alt
        Key::KEY_LEFTALT.code(),
        Key::KEY_RIGHTALT.code(),
        // Windows
        Key::KEY_LEFTMETA.code(),
        Key::KEY_RIGHTMETA.code(),
    ];

    static ref SHIFT_KEYS: [Key; 2] = [
        Key::new(Key::KEY_LEFTSHIFT.code()),
        Key::new(Key::KEY_RIGHTSHIFT.code()),
    ];
    static ref CONTROL_KEYS: [Key; 2] = [
        Key::new(Key::KEY_LEFTCTRL.code()),
        Key::new(Key::KEY_RIGHTCTRL.code()),
    ];
    static ref ALT_KEYS: [Key; 2] = [
        Key::new(Key::KEY_LEFTALT.code()),
        Key::new(Key::KEY_RIGHTALT.code()),
    ];
    static ref WINDOWS_KEYS: [Key; 2] = [
        Key::new(Key::KEY_LEFTMETA.code()),
        Key::new(Key::KEY_RIGHTMETA.code()),
    ];
}

//---

fn is_pressed(value: i32) -> bool {
    value == PRESS || value == REPEAT
}

// InputEvent#value
static RELEASE: i32 = 0;
static PRESS: i32 = 1;
static REPEAT: i32 = 2;

//---

#[derive(Debug)]
struct MultiPurposeKeyState {
    held: Key,
    alone: Key,
    // Some if the first press is still delayed, None if already considered held.
    alone_timeout_at: Option<Instant>,
}

impl MultiPurposeKeyState {
    fn repeat(&mut self) -> Vec<(Key, i32)> {
        if let Some(alone_timeout_at) = &self.alone_timeout_at {
            if Instant::now() < *alone_timeout_at {
                vec![] // still delay the press
            } else {
                self.alone_timeout_at = None; // timeout
                vec![(self.held, PRESS)]
            }
        } else {
            vec![(self.held, REPEAT)]
        }
    }

    fn release(&self) -> Vec<(Key, i32)> {
        if let Some(alone_timeout_at) = &self.alone_timeout_at {
            if Instant::now() < *alone_timeout_at {
                // dispatch the delayed press and this release
                vec![(self.alone, PRESS), (self.alone, RELEASE)]
            } else {
                // too late. dispatch the held key
                vec![(self.held, PRESS), (self.held, RELEASE)]
            }
        } else {
            vec![(self.held, RELEASE)]
        }
    }

    fn force_held(&mut self) -> Vec<(Key, i32)> {
        if self.alone_timeout_at.is_some() {
            self.alone_timeout_at = None;
            vec![(self.held, PRESS)]
        } else {
            vec![]
        }
    }
}
