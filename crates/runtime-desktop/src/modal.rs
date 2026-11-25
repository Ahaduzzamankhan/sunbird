//! Native GPUI / Mana modal overlay, syntax-colorized code viewer, and native
//! `TextInput` / `TextArea` editors for Desktop Phoenix.
//!
//! Hooks Discord's window root render (`modal-layer-scrim` ->
//! `WindowRoot::render`) and composes Discord's own
//! `mana_html::hosted::modal::Modal`, `TextInput`, and `TextArea` natively on
//! top of the active window — no Win32 popup windows and no WebView2 overlays.

use std::{
   ffi::c_void,
   mem::MaybeUninit,
   ptr,
   slice,
   str,
   sync::{
      Arc,
      Condvar,
      LazyLock,
      Mutex,
      OnceLock,
      PoisonError,
      atomic::{
         AtomicBool,
         AtomicU64,
         AtomicUsize,
         Ordering,
      },
   },
   time::Duration,
};

use phoenix_hook::image::Image;
use phoenix_runtime::highlight::{
   self,
   JsonKind,
   Kind,
};
use phoenix_sigs::desktop::{
   self,
   FieldLayout,
   ModalLayout,
   layout,
};

use crate::gpui::{
   self,
   BtnSize,
   BtnVariant,
   RawAnyElement,
   RawButton,
   RawDiv,
   RcFnVtbl,
   Scratch,
   SharedString,
   Style,
   W_FULL,
   argb_to_hsla,
   gpui_px,
   intern_str,
};

const WM_GPUI_FORCE_UPDATE_WINDOW: u32 = 0x0405;
const GWLP_USERDATA: i32 = -21;
const MAX_HIGHLIGHT_LINES: usize = 600;
const MONO_FONT: &str = "gg sans mono";

// GitHub-Dark / Discord syntax palette (ARGB -> HSLA via `argb_to_hsla`).
const COL_PLAIN: u32 = 0xFFE6_EDF3;
const COL_KEY: u32 = 0xFF79_C0FF;
const COL_STRING: u32 = 0xFF7E_E787;
const COL_NUMBER: u32 = 0xFFFF_A657;
const COL_BOOL: u32 = 0xFFD2_A8FF;
const COL_NULL: u32 = 0xFFFF_7B72;
const COL_PUNCT: u32 = 0xFF8B_949E;
const COL_KEYWORD: u32 = 0xFFFF_7B72;
const COL_COMMENT: u32 = 0xFF8B_949E;
const COL_MUTED: u32 = 0xFF94_9BA4;
const COL_ERROR: u32 = 0xFFFF_7B72;
const COL_OK: u32 = 0xFF7E_E787;

// ---------------------------------------------------------------------------
// Public Modal Specs
// ---------------------------------------------------------------------------

/// A clickable action button inside a native GPUI page modal
/// (`phoenix.ui.openPage`).
#[derive(Clone)]
pub struct ModalButton {
   pub label:    String,
   pub danger:   bool,
   pub on_press: Arc<dyn Fn() + Send + Sync>,
}

/// A section inside a native GPUI page modal (`phoenix.ui.openPage`).
#[derive(Clone)]
pub enum ModalSection {
   Buttons {
      title:   Option<String>,
      buttons: Vec<ModalButton>,
   },
   Text {
      title: Option<String>,
      text:  String,
   },
   Code {
      title:     Option<String>,
      text:      String,
      monospace: bool,
      highlight: bool,
   },
}

/// Specification for a native GPUI plugin page (`phoenix.ui.openPage`).
#[derive(Clone)]
pub struct PageModalSpec {
   pub title:    String,
   pub sections: Vec<ModalSection>,
}

/// A native `phoenix.ui.choose` / `showSheet` dialog whose answer a waiting
/// worker thread reads.
pub struct Choice {
   picked: Mutex<Option<i32>>,
   done:   Condvar,
}

impl Choice {
   const fn new() -> Self {
      Self {
         picked: Mutex::new(None),
         done:   Condvar::new(),
      }
   }

   /// Records `index` unless the dialog already answered.
   pub fn pick(&self, index: i32) {
      let mut picked = self.picked.lock().unwrap_or_else(PoisonError::into_inner);
      if picked.is_none() {
         *picked = Some(index);
      }
      drop(picked);
      self.done.notify_all();
   }

   /// Waits for the picked index, or `None` when `timeout` passes first.
   pub fn wait(&self, timeout: Duration) -> Option<i32> {
      let picked = self.picked.lock().unwrap_or_else(PoisonError::into_inner);
      let (picked, _) = self
         .done
         .wait_timeout_while(picked, timeout, |picked| picked.is_none())
         .unwrap_or_else(PoisonError::into_inner);
      *picked
   }
}

/// One button of a native choose dialog.
pub struct ChoiceOption {
   pub label:  String,
   pub danger: bool,
}

#[derive(Clone)]
struct CodeEditorModal {
   plugin:       String,
   pending_init: Option<String>,
   status:       Option<(String, bool)>,
}

#[derive(Clone)]
struct InstallUrlModal {
   pending_init: Option<String>,
   status:       Option<(String, bool)>,
   busy:         bool,
}

#[derive(Clone)]
enum ActiveModal {
   Page {
      id:   u64,
      spec: PageModalSpec,
   },
   Choice {
      spec:   PageModalSpec,
      choice: Arc<Choice>,
   },
   CodeEditor(CodeEditorModal),
   InstallUrl(InstallUrlModal),
}

/// The active modal copied out of the state lock for one render pass. A choose
/// dialog draws as the page it is.
enum Snapshot {
   Page(PageModalSpec),
   CodeEditor(CodeEditorModal),
   InstallUrl(InstallUrlModal),
}

// ---------------------------------------------------------------------------
// ABI Mirrors & Resolved Discord Symbols
// ---------------------------------------------------------------------------

type RawModal = Scratch<{ layout::SCRATCH_CAP }>;
type RawTextInput = Scratch<{ layout::SCRATCH_CAP }>;
type RawTextArea = Scratch<{ layout::SCRATCH_CAP }>;

#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct RawEntity {
   id_lo: u32,
   id_hi: u32,
   rest:  [u64; 3],
}
const _: () = assert!(size_of::<RawEntity>() == layout::ENTITY);

#[repr(C, align(8))]
struct RawContext {
   app:  *mut c_void,
   weak: RawEntity,
}

#[repr(C)]
struct RawVecAny {
   cap: usize,
   ptr: *mut RawAnyElement,
   len: usize,
}

type OverlayRenderFn = unsafe extern "C" fn(
   sret: *mut RawVecAny,
   workspace: *mut c_void,
   arg3: *mut c_void,
   window: *mut c_void,
   cx: *mut c_void,
) -> *mut RawVecAny;

type VecPushAnyFn = unsafe extern "C" fn(*mut RawVecAny, *mut RawAnyElement);

type ModalNewFn = unsafe extern "C" fn(*mut RawModal, *const SharedString) -> *mut RawModal;
type ModalIntoAnyFn = unsafe extern "C" fn(*mut RawAnyElement, *mut RawModal) -> *mut RawAnyElement;
type ModalChildDivFn =
   unsafe extern "C" fn(*mut RawModal, *mut RawModal, *mut RawDiv) -> *mut RawModal;
type ModalFooterBtnFn =
   unsafe extern "C" fn(*mut RawModal, *mut RawModal, *mut RawButton) -> *mut RawModal;
type DivIdFn = unsafe extern "C" fn(*mut RawDiv, *mut RawDiv, *const u8, usize) -> *mut RawDiv;

type CxNewFieldFn =
   unsafe extern "C" fn(*mut RawEntity, *mut c_void, *mut c_void) -> *mut RawEntity;
type EntityMapGetFn = unsafe extern "C" fn(*mut c_void, u32, u32) -> *mut c_void;
type EntityDowngradeFn = unsafe extern "C" fn(*mut RawEntity, *const RawEntity) -> *mut RawEntity;
type WeakDropFn = unsafe extern "C" fn(*mut c_void);
type TextGetFn =
   unsafe extern "C" fn(*mut SharedString, *const c_void, *const c_void) -> *mut SharedString;
type SetValueFn = unsafe extern "C" fn(*mut c_void, *const u8, usize, *mut c_void, *mut c_void);
type UpdateEntityDynFn = unsafe extern "C" fn(
   *mut c_void,
   *const RawEntity,
   *const u8,
   usize,
   *mut c_void,
   *const RcFnVtbl,
);
type ArcDropSlowFn = unsafe extern "C" fn(*mut *const u8);
type TextInputNewFn =
   unsafe extern "C" fn(*mut RawTextInput, *const RawEntity) -> *mut RawTextInput;
type TextInputIntoAnyFn =
   unsafe extern "C" fn(*mut RawAnyElement, *mut RawTextInput) -> *mut RawAnyElement;
type TextAreaNewFn = unsafe extern "C" fn(*mut RawTextArea, *const RawEntity) -> *mut RawTextArea;
type TextAreaIntoAnyFn =
   unsafe extern "C" fn(*mut RawAnyElement, *mut RawTextArea) -> *mut RawAnyElement;
type ListenerPushFn = unsafe extern "C" fn(*mut c_void, *mut u8, *const RcFnVtbl);
type IsHoveredFn = unsafe extern "C" fn(*const c_void, *const c_void) -> bool;

struct ModalSymbols {
   vec_push_any:        VecPushAnyFn,
   modal_new:           ModalNewFn,
   modal_into_any:      ModalIntoAnyFn,
   modal_child_div:     ModalChildDivFn,
   modal_footer_button: ModalFooterBtnFn,
   div_id:              DivIdFn,
   layout:              ModalLayout,
}

/// Click-outside-to-close, wired the way Discord's own overlay `backdrop` is.
struct DismissSymbols {
   push:                 ListenerPushFn,
   is_hovered:           IsHoveredFn,
   /// `Div` offset of its `Box<Interactivity>`.
   interactivity:        usize,
   mouse_down_listeners: usize,
   propagate_event:      usize,
}

struct TextFieldSymbols {
   cx_new_single:       CxNewFieldFn,
   cx_new_multiline:    CxNewFieldFn,
   entity_map_get:      EntityMapGetFn,
   entity_downgrade:    EntityDowngradeFn,
   weak_drop:           WeakDropFn,
   text_get:            TextGetFn,
   set_value:           SetValueFn,
   update_entity_dyn:   UpdateEntityDynFn,
   arc_drop_slow:       ArcDropSlowFn,
   text_input_new:      TextInputNewFn,
   text_input_into_any: TextInputIntoAnyFn,
   text_area_new:       TextAreaNewFn,
   text_area_into_any:  TextAreaIntoAnyFn,
   layout:              FieldLayout,
}

// SAFETY: resolved static function pointers in `Discord.exe`.
unsafe impl Send for ModalSymbols {}
unsafe impl Sync for ModalSymbols {}
unsafe impl Send for TextFieldSymbols {}
unsafe impl Sync for TextFieldSymbols {}
unsafe impl Send for DismissSymbols {}
unsafe impl Sync for DismissSymbols {}

static ORIG_OVERLAY_RENDER: OnceLock<OverlayRenderFn> = OnceLock::new();
static MODAL_SYM: OnceLock<ModalSymbols> = OnceLock::new();
static FIELD_SYM: OnceLock<TextFieldSymbols> = OnceLock::new();
static DISMISS_SYM: OnceLock<DismissSymbols> = OnceLock::new();
/// Set while one mouse press is dispatched if it landed on the modal card.
static PRESS_IN_CARD: AtomicBool = AtomicBool::new(false);
static WINDOW_VAL_OFFSET: AtomicUsize = AtomicUsize::new(0);

static MODAL_OPEN: AtomicBool = AtomicBool::new(false);
static NEXT_PAGE_ID: AtomicU64 = AtomicU64::new(1);
static ACTIVE_MODAL: LazyLock<Mutex<Option<ActiveModal>>> = LazyLock::new(|| Mutex::new(None));

/// Main-thread-owned GPUI `Entity<FieldState>` handles, lazily created on first
/// render when `window` and `app` are live.
struct FieldEntities {
   url:  Option<RawEntity>,
   code: Option<RawEntity>,
}

static FIELD_ENTITIES: LazyLock<Mutex<FieldEntities>> = LazyLock::new(|| {
   Mutex::new(FieldEntities {
      url:  None,
      code: None,
   })
});

#[derive(Clone)]
enum ModalAction {
   Close,
   PageButton(Arc<dyn Fn() + Send + Sync>),
   SaveCode(String),
   CopyCode,
   SubmitInstallUrl,
}

static MODAL_ACTIONS: LazyLock<Mutex<Vec<ModalAction>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn register_modal_action(action: ModalAction) -> u64 {
   let mut list = MODAL_ACTIONS.lock().unwrap_or_else(PoisonError::into_inner);
   let id = list.len() as u64;
   list.push(action);
   id
}

fn lookup_modal_action(id: u64) -> Option<ModalAction> {
   let list = MODAL_ACTIONS.lock().unwrap_or_else(PoisonError::into_inner);
   usize::try_from(id)
      .ok()
      .and_then(|idx| list.get(idx).cloned())
}

// ---------------------------------------------------------------------------
// Vtables for Modal on_close (3 args), Modal Buttons (4 args), & update_entity
// ---------------------------------------------------------------------------

static MODAL_CLOSE_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: ptr::null(),
   size:          size_of::<u64>(),
   align:         align_of::<u64>(),
   call_once:     modal_close_callback as *const c_void,
   call_mut:      modal_close_callback as *const c_void,
   call:          modal_close_callback as *const c_void,
   extra:         ptr::null(),
};

static MODAL_BTN_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: ptr::null(),
   size:          size_of::<u64>(),
   align:         align_of::<u64>(),
   call_once:     modal_button_callback as *const c_void,
   call_mut:      modal_button_callback as *const c_void,
   call:          modal_button_callback as *const c_void,
   extra:         ptr::null(),
};

// `Box<dyn Fn(&MouseDownEvent, DispatchPhase, &Hitbox, &mut Window, &mut App)>`
// vtables for zero-sized listeners, so the box holds no allocation.
static SCRIM_PRESS_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: noop_drop as *const c_void,
   size:          0,
   align:         1,
   call_once:     scrim_press as *const c_void,
   call_mut:      scrim_press as *const c_void,
   call:          scrim_press as *const c_void,
   extra:         ptr::null(),
};

static CARD_PRESS_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: noop_drop as *const c_void,
   size:          0,
   align:         1,
   call_once:     card_press as *const c_void,
   call_mut:      card_press as *const c_void,
   call:          card_press as *const c_void,
   extra:         ptr::null(),
};

const PHASE_BUBBLE: u8 = 0;
const PHASE_CAPTURE: u8 = 1;

const unsafe extern "C" fn noop_drop(_data: *mut u8) {}

/// GPUI dispatches a press front to back in the capture phase, then back to
/// front in the bubble phase, so the card (painted over the scrim) bubbles
/// first. A press the card saw is kept; any other press on the scrim closes.
unsafe extern "C" fn scrim_press(
   _data: *const u8,
   _event: *const c_void,
   phase: u8,
   hitbox: *const c_void,
   window: *const c_void,
   app: *mut u8,
) {
   let _ = std::panic::catch_unwind(|| {
      if phase == PHASE_CAPTURE {
         PRESS_IN_CARD.store(false, Ordering::Relaxed);
         return;
      }
      let Some(sym) = DISMISS_SYM.get() else {
         return;
      };
      // SAFETY: GPUI passes the live `&Hitbox`, `&Window` and `&mut App`.
      if phase != PHASE_BUBBLE || app.is_null() || !unsafe { (sym.is_hovered)(hitbox, window) } {
         return;
      }
      // Like Discord's backdrop, swallow the press so the chat underneath never
      // sees it (`cx.stop_propagation()`).
      // SAFETY: `propagate_event` is a `bool` in `App`, located by `sigs`.
      unsafe { app.add(sym.propagate_event).write(0) };
      if !PRESS_IN_CARD.swap(false, Ordering::Relaxed) {
         close_active();
      }
   });
}

unsafe extern "C" fn card_press(
   _data: *const u8,
   _event: *const c_void,
   phase: u8,
   hitbox: *const c_void,
   window: *const c_void,
   _app: *mut u8,
) {
   let _ = std::panic::catch_unwind(|| {
      if phase == PHASE_BUBBLE
         && let Some(sym) = DISMISS_SYM.get()
         // SAFETY: GPUI passes the live `&Hitbox` and `&Window`.
         && unsafe { (sym.is_hovered)(hitbox, window) }
      {
         PRESS_IN_CARD.store(true, Ordering::Relaxed);
      }
   });
}

/// Pushes a zero-sized listener into `div`'s `Interactivity` mouse-down list,
/// which also makes GPUI give `div` a hitbox.
fn on_mouse_down(div: &mut RawDiv, vtbl: &'static RcFnVtbl) -> bool {
   let Some(sym) = DISMISS_SYM.get() else {
      return false;
   };
   // SAFETY: `RawDiv` holds its `Box<Interactivity>` at `interactivity`
   // (decoded by `sigs`); `push` is Discord's own `Vec<Box<dyn ..>>::push`.
   unsafe {
      let interactivity = div
         .as_mut_ptr()
         .add(sym.interactivity)
         .cast::<*mut u8>()
         .read_unaligned();
      if interactivity.is_null() {
         return false;
      }
      (sym.push)(
         interactivity.add(sym.mouse_down_listeners).cast::<c_void>(),
         ptr::dangling_mut::<u8>(),
         vtbl,
      );
   }
   true
}

#[repr(C)]
struct SetValueEnv {
   entity:   *const RawEntity,
   text_ptr: *const u8,
   text_len: usize,
   tf:       *const TextFieldSymbols,
}

static SET_VALUE_VTBL: RcFnVtbl = RcFnVtbl {
   drop_in_place: ptr::null(),
   size:          size_of::<SetValueEnv>(),
   align:         align_of::<SetValueEnv>(),
   call_once:     set_value_trampoline as *const c_void,
   call_mut:      set_value_trampoline as *const c_void,
   call:          set_value_trampoline as *const c_void,
   extra:         ptr::null(),
};

unsafe extern "C" fn set_value_trampoline(
   env: *mut SetValueEnv,
   field: *mut c_void,
   _any_vtbl: *const c_void,
   app: *mut c_void,
) {
   if env.is_null() || field.is_null() || app.is_null() {
      return;
   }
   let _ = std::panic::catch_unwind(|| {
      // SAFETY: called synchronously from `App::update_entity_dyn` on the GPUI
      // main thread with valid `env`, leased `field`, and `app`.
      unsafe {
         let env_ref = &*env;
         let tf = &*env_ref.tf;
         let mut ctx = MaybeUninit::<RawContext>::uninit();
         let ctx_ptr = ctx.as_mut_ptr();
         (*ctx_ptr).app = app;
         (tf.entity_downgrade)(&raw mut (*ctx_ptr).weak, env_ref.entity);
         let mut ctx = ctx.assume_init();
         // Discord passes a non-null dangling pointer (`1`) for empty slices.
         let ptr = if env_ref.text_len == 0 {
            ptr::dangling()
         } else {
            env_ref.text_ptr
         };
         (tf.set_value)(
            field,
            ptr,
            env_ref.text_len,
            ptr::null_mut(),
            (&raw mut ctx).cast::<c_void>(),
         );
         // `weak_drop` takes the `Weak` inside the `WeakEntity`, as Discord's
         // own `cx.new` drops its context.
         let weak_slot = (&raw mut ctx.weak)
            .cast::<u8>()
            .add(tf.layout.weak_ptr)
            .cast::<c_void>();
         (tf.weak_drop)(weak_slot);
      }
   });
}

/// `Modal::on_close` invokes `Fn::call(&self, window, cx)` — 3 arguments, no
/// click event argument.
unsafe extern "C" fn modal_close_callback(
   _self_ptr: *const u64,
   _window: *mut c_void,
   _cx: *mut c_void,
) {
   let _ = std::panic::catch_unwind(close_active);
}

/// Mana `Button::on_click` invokes `Fn::call(&self, event, window, app)` — 4
/// arguments, giving us a live `*mut App` for reading native `FieldState`s.
unsafe extern "C" fn modal_button_callback(
   self_ptr: *const u64,
   _event: *const c_void,
   _window: *mut c_void,
   app: *mut c_void,
) {
   if self_ptr.is_null() {
      return;
   }
   let _ = std::panic::catch_unwind(|| {
      // SAFETY: `self_ptr` points to `RcInner.action_id`.
      let id = unsafe { *self_ptr };
      let Some(action) = lookup_modal_action(id) else {
         return;
      };
      match action {
         ModalAction::Close => close_active(),
         ModalAction::PageButton(cb) => {
            std::thread::spawn(move || cb());
         },
         ModalAction::CopyCode => {
            if let Some(code) = read_code_field(app) {
               match phoenix_ui_desktop::copy(&code) {
                  Ok(()) => phoenix_ui_desktop::toast("Copied plugin source"),
                  Err(err) => phoenix_ui_desktop::toast(&format!("Copy failed: {err}")),
               }
            }
         },
         ModalAction::SaveCode(plugin) => {
            let Some(code) = read_code_field(app) else {
               set_editor_status("Could not read text field", true);
               return;
            };
            match phoenix_runtime::save_source(&plugin, code) {
               Ok(()) => {
                  close_active();
                  phoenix_ui_desktop::toast(&format!("Saved & reloaded {plugin}"));
               },
               Err(err) => {
                  set_editor_status(&format!("Save failed: {err}"), true);
               },
            }
         },
         ModalAction::SubmitInstallUrl => {
            let Some(raw) = read_url_field(app) else {
               set_install_status("Could not read URL field", true, false);
               return;
            };
            let url = raw.trim().to_owned();
            if url.is_empty() {
               set_install_status("Enter an https:// URL ending in .js", true, false);
               return;
            }
            set_install_status("Downloading and installing plugin...", false, true);
            std::thread::spawn(move || {
               match phoenix_runtime::install(&url) {
                  Ok(name) => {
                     close_active();
                     phoenix_ui_desktop::toast(&format!("Installed {name}"));
                  },
                  Err(err) => {
                     set_install_status(&format!("{err}"), true, false);
                  },
               }
            });
         },
      }
   });
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Opens a native GPUI modal for `phoenix.ui.openPage` and triggers a GPUI
/// redraw immediately.
pub fn open_page(spec: PageModalSpec) -> u64 {
   resolve_pending_choice();
   let id = NEXT_PAGE_ID.fetch_add(1, Ordering::Relaxed);
   *ACTIVE_MODAL.lock().unwrap_or_else(PoisonError::into_inner) =
      Some(ActiveModal::Page { id, spec });
   MODAL_OPEN.store(true, Ordering::Release);
   force_redraw();
   id
}

/// Opens a native choose or sheet dialog and returns what it will answer.
/// A `showSheet` body is a [`ModalSection::Code`]; a `choose` message is
/// [`ModalSection::Text`]. Without resolved GPUI modal symbols the dialog
/// cannot be drawn, so it answers -1 immediately.
pub fn open_choice(
   title: String,
   body: Option<ModalSection>,
   options: Vec<ChoiceOption>,
) -> Arc<Choice> {
   let choice = Arc::new(Choice::new());
   if MODAL_SYM.get().is_none() {
      choice.pick(-1);
      return choice;
   }
   resolve_pending_choice();

   let mut sections = Vec::with_capacity(2);
   if let Some(body) = body {
      sections.push(body);
   }
   if !options.is_empty() {
      let buttons = options
         .into_iter()
         .enumerate()
         .map(|(index, option)| {
            let picked = Arc::clone(&choice);
            ModalButton {
               label:    option.label,
               danger:   option.danger,
               on_press: Arc::new(move || {
                  picked.pick(i32::try_from(index).unwrap_or(-1));
                  close_active();
               }),
            }
         })
         .collect();
      sections.push(ModalSection::Buttons {
         title: None,
         buttons,
      });
   }
   *ACTIVE_MODAL.lock().unwrap_or_else(PoisonError::into_inner) = Some(ActiveModal::Choice {
      spec:   PageModalSpec { title, sections },
      choice: Arc::clone(&choice),
   });
   MODAL_OPEN.store(true, Ordering::Release);
   force_redraw();
   choice
}

/// Closes `choice`'s dialog if it is still the one on screen.
pub fn dismiss_choice(choice: &Arc<Choice>) {
   let mut guard = ACTIVE_MODAL.lock().unwrap_or_else(PoisonError::into_inner);
   if matches!(guard.as_ref(), Some(ActiveModal::Choice { choice: active, .. }) if Arc::ptr_eq(active, choice))
   {
      choice.pick(-1);
      *guard = None;
      MODAL_OPEN.store(false, Ordering::Release);
      drop(guard);
      force_redraw();
   }
}

/// Answers any open choose dialog with -1 before it is replaced.
fn resolve_pending_choice() {
   let guard = ACTIVE_MODAL.lock().unwrap_or_else(PoisonError::into_inner);
   if let Some(ActiveModal::Choice { choice, .. }) = guard.as_ref() {
      choice.pick(-1);
   }
}

/// Closes an open page modal if its id still matches `id`.
pub fn close_page(id: u64) {
   let mut guard = ACTIVE_MODAL.lock().unwrap_or_else(PoisonError::into_inner);
   if matches!(&*guard, Some(ActiveModal::Page { id: cur, .. }) if *cur == id) {
      *guard = None;
      MODAL_OPEN.store(false, Ordering::Release);
      drop(guard);
      force_redraw();
   }
}

/// Opens a native GPUI multiline code editor modal for `plugin`.
pub fn open_code_editor(plugin: String, initial_source: String) {
   resolve_pending_choice();
   *ACTIVE_MODAL.lock().unwrap_or_else(PoisonError::into_inner) =
      Some(ActiveModal::CodeEditor(CodeEditorModal {
         plugin,
         pending_init: Some(initial_source),
         status: None,
      }));
   MODAL_OPEN.store(true, Ordering::Release);
   force_redraw();
}

/// Opens a native GPUI single-line URL installer modal.
pub fn open_install_url() {
   resolve_pending_choice();
   *ACTIVE_MODAL.lock().unwrap_or_else(PoisonError::into_inner) =
      Some(ActiveModal::InstallUrl(InstallUrlModal {
         pending_init: Some(String::new()),
         status:       None,
         busy:         false,
      }));
   MODAL_OPEN.store(true, Ordering::Release);
   force_redraw();
}

fn close_active() {
   let mut guard = ACTIVE_MODAL.lock().unwrap_or_else(PoisonError::into_inner);
   if let Some(ActiveModal::Choice { choice, .. }) = guard.as_ref() {
      choice.pick(-1);
   }
   *guard = None;
   drop(guard);
   MODAL_OPEN.store(false, Ordering::Release);
   force_redraw();
}

fn set_editor_status(msg: &str, is_error: bool) {
   let mut guard = ACTIVE_MODAL.lock().unwrap_or_else(PoisonError::into_inner);
   if let Some(ActiveModal::CodeEditor(ed)) = guard.as_mut() {
      ed.status = Some((msg.to_owned(), is_error));
   }
   drop(guard);
   force_redraw();
}

fn set_install_status(msg: &str, is_error: bool, busy: bool) {
   let mut guard = ACTIVE_MODAL.lock().unwrap_or_else(PoisonError::into_inner);
   if let Some(ActiveModal::InstallUrl(inst)) = guard.as_mut() {
      inst.status = Some((msg.to_owned(), is_error));
      inst.busy = busy;
   }
   drop(guard);
   force_redraw();
}

// ---------------------------------------------------------------------------
// GPUI Force-Redraw via `WM_GPUI_FORCE_UPDATE_WINDOW` (`WM_USER + 5`)
// ---------------------------------------------------------------------------

#[link(name = "user32")]
unsafe extern "system" {
   fn EnumWindows(cb: unsafe extern "system" fn(isize, isize) -> i32, lparam: isize) -> i32;
   fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
   fn IsWindowVisible(hwnd: isize) -> i32;
   fn GetClassNameW(hwnd: isize, buf: *mut u16, max_count: i32) -> i32;
   fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
   fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
   fn InvalidateRect(hwnd: isize, rect: *const c_void, erase: i32) -> i32;
}

unsafe extern "system" fn redraw_zed_window(hwnd: isize, lparam: isize) -> i32 {
   let want_pid = u32::try_from(lparam).unwrap_or_default();
   let mut pid = 0u32;
   // SAFETY: Win32 window queries for this process's own visible `Zed::Window`s.
   unsafe {
      GetWindowThreadProcessId(hwnd, &raw mut pid);
      if pid != want_pid || IsWindowVisible(hwnd) == 0 {
         return 1;
      }
      let val_off = WINDOW_VAL_OFFSET.load(Ordering::Acquire);
      if val_off != 0 {
         let mut cls = [0u16; 32];
         let n = GetClassNameW(hwnd, cls.as_mut_ptr(), 32);
         if n > 0 && String::from_utf16_lossy(&cls[..n as usize]) == "Zed::Window" {
            let user_data = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as usize;
            if user_data != 0 && user_data != usize::MAX {
               let rc = *(user_data as *const usize);
               if rc != 0 && rc != usize::MAX {
                  let strong = *(rc as *const usize);
                  if strong != 0 {
                     let validation = *((rc + val_off) as *const usize);
                     PostMessageW(hwnd, WM_GPUI_FORCE_UPDATE_WINDOW, validation, 0);
                     return 1;
                  }
               }
            }
         }
      }
      InvalidateRect(hwnd, ptr::null(), 0);
   }
   1
}

/// Forces every visible GPUI window in this process to re-render immediately
/// (works reliably from any background thread, unlike `InvalidateRect` alone).
pub fn force_redraw() {
   let pid = std::process::id();
   // SAFETY: enumerates windows in this process and posts `WM_USER + 5` with
   // GPUI's validated window token.
   unsafe {
      EnumWindows(redraw_zed_window, isize::try_from(pid).unwrap_or_default());
   }
}

// ---------------------------------------------------------------------------
// Installation (`DllMain` / `crate::prepare`)
// ---------------------------------------------------------------------------

/// Locates Discord's native `Modal`, `Div::id`, `TextInput`/`TextArea`, and
/// window root render, then hooks `WindowRoot::render`.
#[expect(clippy::too_many_lines, reason = "one symbol table per located group")]
pub fn install(image: &Image<'static>) {
   let (Some(div_into_any), Some(gpui_layout)) = (gpui::div_into_any_addr(), gpui::layout()) else {
      log::warn!("desktop modal: GPUI builders unavailable, skipping");
      return;
   };
   let found = match desktop::modal(image, div_into_any, &gpui_layout.div) {
      Ok(m) => m,
      Err(err) => {
         log::warn!("desktop modal: {err}, native modals disabled");
         return;
      },
   };
   WINDOW_VAL_OFFSET.store(found.window_validation_offset, Ordering::Release);
   // SAFETY: resolved and shape-checked against `Discord.exe` by `sigs`.
   let _ = DISMISS_SYM.set(unsafe {
      DismissSymbols {
         push:                 std::mem::transmute::<usize, ListenerPushFn>(
            found.dismiss.listener_push,
         ),
         is_hovered:           std::mem::transmute::<usize, IsHoveredFn>(
            found.dismiss.hitbox_is_hovered,
         ),
         interactivity:        gpui_layout.div.interactivity,
         mouse_down_listeners: found.dismiss.mouse_down_listeners,
         propagate_event:      found.dismiss.propagate_event,
      }
   });

   // SAFETY: resolved and layout-verified against `Discord.exe`.
   let modal_sym = unsafe {
      ModalSymbols {
         vec_push_any:        std::mem::transmute::<usize, VecPushAnyFn>(found.vec_push_any),
         modal_new:           std::mem::transmute::<usize, ModalNewFn>(found.modal_new),
         modal_into_any:      std::mem::transmute::<usize, ModalIntoAnyFn>(found.modal_into_any),
         modal_child_div:     std::mem::transmute::<usize, ModalChildDivFn>(found.modal_child_div),
         modal_footer_button: std::mem::transmute::<usize, ModalFooterBtnFn>(
            found.modal_footer_button,
         ),
         div_id:              std::mem::transmute::<usize, DivIdFn>(found.div_id),
         layout:              found.layout,
      }
   };
   let _ = MODAL_SYM.set(modal_sym);

   match desktop::text_fields(image) {
      Ok(tf) => {
         // SAFETY: resolved and cross-checked against `Discord.exe`.
         let sym = unsafe {
            TextFieldSymbols {
               cx_new_single:       std::mem::transmute::<usize, CxNewFieldFn>(tf.cx_new_single),
               cx_new_multiline:    std::mem::transmute::<usize, CxNewFieldFn>(tf.cx_new_multiline),
               entity_map_get:      std::mem::transmute::<usize, EntityMapGetFn>(tf.entity_map_get),
               entity_downgrade:    std::mem::transmute::<usize, EntityDowngradeFn>(
                  tf.entity_downgrade,
               ),
               weak_drop:           std::mem::transmute::<usize, WeakDropFn>(tf.weak_drop),
               text_get:            std::mem::transmute::<usize, TextGetFn>(tf.text_get),
               set_value:           std::mem::transmute::<usize, SetValueFn>(tf.set_value),
               update_entity_dyn:   std::mem::transmute::<usize, UpdateEntityDynFn>(
                  tf.update_entity_dyn,
               ),
               arc_drop_slow:       std::mem::transmute::<usize, ArcDropSlowFn>(tf.arc_drop_slow),
               text_input_new:      std::mem::transmute::<usize, TextInputNewFn>(tf.text_input_new),
               text_input_into_any: std::mem::transmute::<usize, TextInputIntoAnyFn>(
                  tf.text_input_into_any,
               ),
               text_area_new:       std::mem::transmute::<usize, TextAreaNewFn>(tf.text_area_new),
               text_area_into_any:  std::mem::transmute::<usize, TextAreaIntoAnyFn>(
                  tf.text_area_into_any,
               ),
               layout:              tf.layout,
            }
         };
         let _ = FIELD_SYM.set(sym);
         log::info!(
            "desktop modal: resolved native TextInput/TextArea (single={:#x}, multi={:#x}) {:x?}",
            tf.cx_new_single,
            tf.cx_new_multiline,
            tf.layout,
         );
      },
      Err(err) => {
         log::warn!("desktop modal: native TextInput/TextArea unavailable: {err}");
      },
   }

   // SAFETY: runs from `DllMain` before Discord threads execute;
   // `overlay_render_detour` matches `Workspace::render_overlays`'s 5-arg sret
   // ABI returning `Vec<AnyElement>`.
   let trampoline = match unsafe {
      image.hook_entry(
         found.overlay_render,
         desktop::FRAME_PROLOGUE,
         desktop::FRAME_PROLOGUE_LEN,
         overlay_render_detour as *const () as usize,
      )
   } {
      Ok(trampoline) => trampoline,
      Err(err) => {
         log::warn!("desktop modal: overlay render hook failed: {err}");
         return;
      },
   };
   // SAFETY: `trampoline` replays the 19-byte prologue and jumps to `+19`.
   let orig = unsafe { std::mem::transmute::<usize, OverlayRenderFn>(trampoline) };
   let _ = ORIG_OVERLAY_RENDER.set(orig);
   log::info!(
      "desktop modal: hooked Workspace::render_overlays @ {:#x} (push={:#x}, val +{:#x}) {:x?}",
      found.overlay_render,
      found.vec_push_any,
      found.window_validation_offset,
      found.layout,
   );
}

// ---------------------------------------------------------------------------
// Native FieldState Helpers (GPUI Main Thread Only)
// ---------------------------------------------------------------------------

/// Lazily initializes a single-line or multi-line `Entity<FieldState>` on the
/// GPUI main thread and writes its static placeholder.
unsafe fn ensure_field_entity(
   app: *mut c_void,
   window: *mut c_void,
   multiline: bool,
   placeholder: &'static str,
) -> Option<RawEntity> {
   let tf = FIELD_SYM.get()?;
   if app.is_null() || window.is_null() {
      return None;
   }
   let mut guard = FIELD_ENTITIES
      .lock()
      .unwrap_or_else(PoisonError::into_inner);
   let slot = if multiline {
      &mut guard.code
   } else {
      &mut guard.url
   };
   if let Some(ent) = *slot {
      return Some(ent);
   }
   // SAFETY: invoked on the GPUI main thread inside `WindowRoot::render` with
   // valid `app` and `window` pointers.
   let ent = unsafe {
      let mut out = MaybeUninit::<RawEntity>::uninit();
      if multiline {
         (tf.cx_new_multiline)(out.as_mut_ptr(), app, window);
      } else {
         (tf.cx_new_single)(out.as_mut_ptr(), app, window);
      }
      let ent = out.assume_init();
      let field = (tf.entity_map_get)(
         app.cast::<u8>()
            .add(tf.layout.app_entities)
            .cast::<c_void>(),
         ent.id_lo,
         ent.id_hi,
      );
      if !field.is_null() {
         let ph = SharedString::from_static(placeholder);
         for slot in tf.layout.placeholders {
            field
               .cast::<u8>()
               .add(slot)
               .cast::<SharedString>()
               .write_unaligned(ph);
         }
      }
      ent
   };
   *slot = Some(ent);
   Some(ent)
}

unsafe fn set_field_text(app: *mut c_void, entity: &RawEntity, text: &str) {
   let Some(tf) = FIELD_SYM.get() else {
      return;
   };
   if app.is_null() {
      return;
   }
   let mut env = SetValueEnv {
      entity:   entity as *const RawEntity,
      text_ptr: text.as_ptr(),
      text_len: text.len(),
      tf:       tf as *const TextFieldSymbols,
   };
   // SAFETY: `update_entity_dyn` leases `FieldState`, calls `set_value_trampoline`
   // synchronously on this stack frame, ends the lease, and flushes effects.
   unsafe {
      (tf.update_entity_dyn)(
         app,
         entity as *const RawEntity,
         b"FieldState".as_ptr(),
         10,
         (&raw mut env).cast::<c_void>(),
         &raw const SET_VALUE_VTBL,
      );
   }
}

unsafe fn read_field_text(app: *mut c_void, entity: &RawEntity) -> Option<String> {
   let tf = FIELD_SYM.get()?;
   if app.is_null() {
      return None;
   }
   // SAFETY: invoked synchronously from a GPUI button callback with live `app`.
   unsafe {
      let field = (tf.entity_map_get)(
         app.cast::<u8>()
            .add(tf.layout.app_entities)
            .cast::<c_void>(),
         entity.id_lo,
         entity.id_hi,
      );
      if field.is_null() {
         return None;
      }
      let mut ss = MaybeUninit::<SharedString>::uninit();
      (tf.text_get)(ss.as_mut_ptr(), field, app);
      let mut ss = ss.assume_init();
      let result = decode_shared_string(&ss);
      if ss.tag >= 0x19 && !ss.ptr.is_null() {
         let strong = &*ss.ptr.cast::<AtomicUsize>();
         if strong.fetch_sub(1, Ordering::Release) == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            (tf.arc_drop_slow)(&raw mut ss.ptr);
         }
      }
      Some(result)
   }
}

unsafe fn decode_shared_string(ss: &SharedString) -> String {
   let tag = ss.tag;
   if tag <= 0x17 {
      // SAFETY: inline bytes live at offsets `1..=tag` inside `SharedString`.
      let bytes = unsafe {
         slice::from_raw_parts(
            (ss as *const SharedString).cast::<u8>().add(1),
            usize::from(tag),
         )
      };
      return String::from_utf8_lossy(bytes).into_owned();
   }
   if tag == 0x18 {
      if ss.ptr.is_null() || ss.len == 0 {
         return String::new();
      }
      // SAFETY: static slice `ptr..ptr + len`.
      let bytes = unsafe { slice::from_raw_parts(ss.ptr, ss.len) };
      return String::from_utf8_lossy(bytes).into_owned();
   }
   if tag == 0x19 {
      if ss.ptr.is_null() || ss.len == 0 {
         return String::new();
      }
      // SAFETY: `ArcInner` has `strong: usize` (+0), `weak: usize` (+8), UTF-8
      // payload at `+0x10` for `ss.len` bytes.
      let bytes = unsafe { slice::from_raw_parts(ss.ptr.add(0x10), ss.len) };
      return String::from_utf8_lossy(bytes).into_owned();
   }
   String::new()
}

fn read_code_field(app: *mut c_void) -> Option<String> {
   let ent = FIELD_ENTITIES
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .code?;
   // SAFETY: called from GPUI button callback with live `app`.
   unsafe { read_field_text(app, &ent) }
}

fn read_url_field(app: *mut c_void) -> Option<String> {
   let ent = FIELD_ENTITIES
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .url?;
   // SAFETY: called from GPUI button callback with live `app`.
   unsafe { read_field_text(app, &ent) }
}

// ---------------------------------------------------------------------------
// Workspace::render_overlays Detour & Native Modal Builder
// ---------------------------------------------------------------------------

unsafe extern "C" fn overlay_render_detour(
   sret: *mut RawVecAny,
   workspace: *mut c_void,
   arg3: *mut c_void,
   window: *mut c_void,
   cx: *mut c_void,
) -> *mut RawVecAny {
   let Some(orig) = ORIG_OVERLAY_RENDER.get().copied() else {
      return sret;
   };
   // SAFETY: always render Discord's own overlay stack first.
   let out = unsafe { orig(sret, workspace, arg3, window, cx) };
   if !MODAL_OPEN.load(Ordering::Acquire) || sret.is_null() || cx.is_null() {
      return out;
   }

   let _ = std::panic::catch_unwind(|| {
      // SAFETY: `[cx]` is `*mut App` inside `Context<Workspace>`.
      let app = unsafe { *(cx.cast::<*mut c_void>()) };
      push_overlay_into(sret, window, app);
   });
   out
}

const WHITE_SPACE_NOWRAP: u8 = 1;
const FLEX_NOWRAP: u8 = 0;
const FLEX_WRAP: u8 = 1;
const OVERFLOW_SCROLL: u8 = 3;
const POSITION_ABSOLUTE: u8 = 1;
const DISPLAY_FLEX: u8 = 1;
const ALIGN_CENTER: u8 = 4;

fn set_mono_font(s: &Style, font_px: f32, line_px: f32) {
   s.length(s.at.font_size, gpui_px(font_px));
   s.length(s.at.line_height, gpui_px(line_px));
   s.shared_string(s.at.font_family, SharedString::from_static(MONO_FONT));
   s.set(s.at.white_space, WHITE_SPACE_NOWRAP);
}

fn div_with_id(msym: &ModalSymbols, mut div: RawDiv, id: &str) -> RawDiv {
   let id_s = intern_str(id);
   let mut out = RawDiv::zeroed();
   // SAFETY: `Div::id` moves `div` into `out` and enables native mouse-wheel
   // scrolling via `Interactivity.element_id`.
   unsafe { (msym.div_id)(&raw mut out, &raw mut div, id_s.as_ptr(), id_s.len()) };
   out
}

fn make_modal_button(
   id: &str,
   label: &str,
   variant: BtnVariant,
   size: BtnSize,
   action: ModalAction,
) -> Option<RawButton> {
   let action_id = register_modal_action(action);
   gpui::build_raw_button(
      id,
      label,
      variant,
      size,
      &raw const MODAL_BTN_VTBL,
      action_id,
   )
}

fn push_modal_button(
   parent: RawDiv,
   id: &str,
   label: &str,
   variant: BtnVariant,
   size: BtnSize,
   action: ModalAction,
) -> RawDiv {
   let Some(btn) = make_modal_button(id, label, variant, size, action) else {
      return parent;
   };
   gpui::push_raw_button(parent, btn)
}

fn mono_span(text: &str, argb: u32) -> Option<RawDiv> {
   let mut d = gpui::raw_div()?;
   let hsla = argb_to_hsla(argb);
   gpui::style_div(&mut d, |s| {
      set_mono_font(s, 12.5, 18.0);
      s.hsla(s.at.text_color, hsla);
      // Prevent token spans from shrinking inside flex-row lines.
      s.float(s.at.flex_shrink, 0.0);
   });
   let mut any = gpui::str_to_any(text)?;
   if !gpui::push_any(&mut d, &mut any) {
      return None;
   }
   Some(d)
}

fn plain_text_div(text: &str, argb: u32, font_px: f32) -> Option<RawDiv> {
   let mut d = gpui::raw_div()?;
   let hsla = argb_to_hsla(argb);
   gpui::style_div(&mut d, |s| {
      s.length(s.at.font_size, gpui_px(font_px));
      s.length(s.at.line_height, gpui_px(font_px + 5.0));
      s.hsla(s.at.text_color, hsla);
   });
   let mut any = gpui::str_to_any(text)?;
   if !gpui::push_any(&mut d, &mut any) {
      return None;
   }
   Some(d)
}

/// Joins same-colored neighbors and folds whitespace into a neighbor. Every
/// span is a full GPUI `Div` rebuilt each frame, so a JSON line drops from
/// about six to three or four.
fn coalesce<'a>(tokens: impl Iterator<Item = (u32, &'a str)>) -> Vec<(u32, String)> {
   let mut spans: Vec<(u32, String)> = Vec::new();
   for (color, tok) in tokens {
      let blank = tok.trim().is_empty();
      match spans.last_mut() {
         Some((last, text)) if blank || *last == color || text.trim().is_empty() => {
            if !blank {
               *last = color;
            }
            text.push_str(tok);
         },
         _ => spans.push((color, tok.to_owned())),
      }
   }
   spans
}

fn build_code_box(
   msym: &ModalSymbols,
   box_id: &str,
   text: &str,
   monospace: bool,
   do_highlight: bool,
) -> Option<RawDiv> {
   let mut box_div = gpui::v_stack(1.0)?;
   box_div = div_with_id(msym, box_div, box_id);
   gpui::style_div(&mut box_div, |s| {
      // Dark Discord code-block background + border + horizontal scroll.
      s.solid_background([0.62, 0.10, 0.08, 1.0]);
      s.lengths(&s.at.border_widths, gpui_px(1.0));
      s.lengths(&s.at.corner_radii, gpui_px(8.0));
      s.lengths(&s.at.padding, gpui_px(12.0));
      s.length(s.at.size[0], W_FULL);
      s.hsla(s.at.border_color, [0.62, 0.08, 0.45, 0.25]);
      s.set(s.at.overflow_x, OVERFLOW_SCROLL);
      if monospace {
         set_mono_font(s, 12.5, 18.0);
      }
   });

   let is_json = do_highlight && highlight::looks_like_json(text);
   for (idx, line) in text.split('\n').enumerate() {
      let line = line.strip_suffix('\r').unwrap_or(line);
      if line.is_empty() {
         if let Some(sp) = mono_span(" ", COL_PLAIN) {
            box_div = gpui::push_div(box_div, sp);
         }
         continue;
      }
      if do_highlight && idx < MAX_HIGHLIGHT_LINES {
         let Some(mut row) = gpui::h_row(0.0, false) else {
            continue;
         };
         gpui::style_div(&mut row, |s| {
            s.set(s.at.white_space, WHITE_SPACE_NOWRAP);
            s.set(s.at.flex_wrap, FLEX_NOWRAP);
         });
         let spans = if is_json {
            coalesce(highlight::json(line).into_iter().map(|(kind, tok)| {
               let color = match kind {
                  JsonKind::Key => COL_KEY,
                  JsonKind::String => COL_STRING,
                  JsonKind::Number => COL_NUMBER,
                  JsonKind::Bool => COL_BOOL,
                  JsonKind::Null => COL_NULL,
                  JsonKind::Punctuation => COL_PUNCT,
                  JsonKind::Plain => COL_PLAIN,
               };
               (color, tok)
            }))
         } else {
            coalesce(highlight::js(line).into_iter().map(|(kind, tok)| {
               let color = match kind {
                  Kind::Keyword => COL_KEYWORD,
                  Kind::String => COL_STRING,
                  Kind::Number => COL_NUMBER,
                  Kind::Comment => COL_COMMENT,
                  Kind::Plain => COL_PLAIN,
               };
               (color, tok)
            }))
         };
         for (color, text) in &spans {
            if let Some(span) = mono_span(text, *color) {
               row = gpui::push_div(row, span);
            }
         }
         box_div = gpui::push_div(box_div, row);
      } else if monospace {
         if let Some(span) = mono_span(line, COL_PLAIN) {
            box_div = gpui::push_div(box_div, span);
         }
      } else if let Some(div) = plain_text_div(line, COL_PLAIN, 13.0) {
         box_div = gpui::push_div(box_div, div);
      }
   }
   Some(box_div)
}

fn build_page_body(msym: &ModalSymbols, spec: &PageModalSpec) -> Option<RawDiv> {
   let mut body = gpui::v_stack(14.0)?;
   gpui::style_div(&mut body, |s| s.length(s.at.size[0], W_FULL));
   for (sec_idx, section) in spec.sections.iter().enumerate() {
      match section {
         ModalSection::Buttons { title, buttons } => {
            let mut block = gpui::v_stack(8.0)?;
            if let Some(t) = title
               && !t.is_empty()
               && let Some(hdr) = plain_text_div(t, COL_MUTED, 12.0)
            {
               block = gpui::push_div(block, hdr);
            }
            let mut row = gpui::h_row(8.0, false)?;
            gpui::style_div(&mut row, |s| s.set(s.at.flex_wrap, FLEX_WRAP));
            for (btn_idx, b) in buttons.iter().enumerate() {
               row = push_modal_button(
                  row,
                  &format!("sunbird-modal-btn-{sec_idx}-{btn_idx}"),
                  &b.label,
                  if b.danger {
                     BtnVariant::Critical
                  } else {
                     BtnVariant::Secondary
                  },
                  BtnSize::Small,
                  ModalAction::PageButton(Arc::clone(&b.on_press)),
               );
            }
            block = gpui::push_div(block, row);
            body = gpui::push_div(body, block);
         },
         ModalSection::Text { title, text } => {
            let mut block = gpui::v_stack(6.0)?;
            if let Some(t) = title
               && !t.is_empty()
               && let Some(hdr) = plain_text_div(t, COL_MUTED, 12.0)
            {
               block = gpui::push_div(block, hdr);
            }
            for line in text.lines() {
               if let Some(d) = plain_text_div(line, COL_PLAIN, 13.5) {
                  block = gpui::push_div(block, d);
               }
            }
            body = gpui::push_div(body, block);
         },
         ModalSection::Code {
            title,
            text,
            monospace,
            highlight,
         } => {
            let mut block = gpui::v_stack(6.0)?;
            if let Some(t) = title
               && !t.is_empty()
               && let Some(hdr) = plain_text_div(t, COL_MUTED, 12.0)
            {
               block = gpui::push_div(block, hdr);
            }
            if let Some(code_box) = build_code_box(
               msym,
               &format!("sunbird-code-{sec_idx}"),
               text,
               *monospace,
               *highlight,
            ) {
               block = gpui::push_div(block, code_box);
            }
            body = gpui::push_div(body, block);
         },
      }
   }
   Some(body)
}

#[expect(
   clippy::too_many_lines,
   reason = "assembles the single active native Mana Modal and scrim overlay"
)]
fn push_overlay_into(overlays: *mut RawVecAny, window: *mut c_void, app: *mut c_void) {
   let Some(msym) = MODAL_SYM.get() else {
      return;
   };

   // Snapshot active modal & take any one-shot `pending_init` text to push into
   // the native GPUI `FieldState`.
   let modal_snapshot = {
      let mut guard = ACTIVE_MODAL.lock().unwrap_or_else(PoisonError::into_inner);
      let Some(active) = guard.as_mut() else {
         MODAL_OPEN.store(false, Ordering::Release);
         return;
      };
      match active {
         ActiveModal::Page { spec, .. } | ActiveModal::Choice { spec, .. } => {
            Snapshot::Page(spec.clone())
         },
         ActiveModal::CodeEditor(ed) => {
            let init = ed.pending_init.take();
            Snapshot::CodeEditor(CodeEditorModal {
               plugin:       ed.plugin.clone(),
               pending_init: init,
               status:       ed.status.clone(),
            })
         },
         ActiveModal::InstallUrl(inst) => {
            let init = inst.pending_init.take();
            Snapshot::InstallUrl(InstallUrlModal {
               pending_init: init,
               status:       inst.status.clone(),
               busy:         inst.busy,
            })
         },
      }
   };

   MODAL_ACTIONS
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .clear();

   let (title, subtitle, size_code, body_opt, footer_buttons): (
      String,
      Option<String>,
      u8,
      Option<RawDiv>,
      Vec<RawButton>,
   ) = match modal_snapshot {
      Snapshot::Page(spec) => {
         let body = build_page_body(msym, &spec);
         let mut footer = Vec::new();
         if let Some(close_btn) = make_modal_button(
            "sunbird-modal-close",
            "Close",
            BtnVariant::Secondary,
            BtnSize::Medium,
            ModalAction::Close,
         ) {
            footer.push(close_btn);
         }
         (spec.title, None, 3, body, footer)
      },
      Snapshot::CodeEditor(ed) => {
         let mut body = gpui::v_stack(10.0);
         if let Some(ref mut b) = body {
            gpui::style_div(b, |s| s.length(s.at.size[0], W_FULL));
            if let Some((msg, is_err)) = &ed.status
               && let Some(st) = plain_text_div(msg, if *is_err { COL_ERROR } else { COL_OK }, 13.0)
            {
               *b = gpui::push_div(std::mem::replace(b, gpui::raw_div().unwrap()), st);
            }
            if let Some(tf) = FIELD_SYM.get()
               && let Some(ent) = unsafe {
                  ensure_field_entity(app, window, true, "// Plugin JavaScript source...")
               }
            {
               if let Some(init) = &ed.pending_init {
                  unsafe { set_field_text(app, &ent, init) };
               }
               // SAFETY: constructs hosted `TextArea` from `&Entity<FieldState>`
               // in a buffer that fits it and converts to `AnyElement`.
               let mut ta_any = unsafe {
                  let mut ta = RawTextArea::zeroed();
                  (tf.text_area_new)(&raw mut ta, &raw const ent);
                  let mut any = MaybeUninit::<RawAnyElement>::uninit();
                  (tf.text_area_into_any)(any.as_mut_ptr(), &raw mut ta);
                  any.assume_init()
               };
               if let Some(mut wrap) = gpui::v_stack(4.0) {
                  gpui::style_div(&mut wrap, |s| {
                     s.length(s.at.size[0], W_FULL);
                     s.length(s.at.min_size[1], gpui_px(380.0));
                     s.shared_string(s.at.font_family, SharedString::from_static(MONO_FONT));
                     s.length(s.at.font_size, gpui_px(13.0));
                  });
                  if gpui::push_any(&mut wrap, &mut ta_any) {
                     *b = gpui::push_div(std::mem::replace(b, gpui::raw_div().unwrap()), wrap);
                  }
               }
            } else if let Some(msg) = plain_text_div(
               "Native TextArea could not be resolved on this Discord build.",
               COL_ERROR,
               13.5,
            ) {
               *b = gpui::push_div(std::mem::replace(b, gpui::raw_div().unwrap()), msg);
            }
         }
         let mut footer = Vec::new();
         if let Some(btn) = make_modal_button(
            "sunbird-edit-cancel",
            "Cancel",
            BtnVariant::Secondary,
            BtnSize::Medium,
            ModalAction::Close,
         ) {
            footer.push(btn);
         }
         if let Some(btn) = make_modal_button(
            "sunbird-edit-copy",
            "Copy Source",
            BtnVariant::Secondary,
            BtnSize::Medium,
            ModalAction::CopyCode,
         ) {
            footer.push(btn);
         }
         if let Some(btn) = make_modal_button(
            "sunbird-edit-save",
            "Save & Reload",
            BtnVariant::Primary,
            BtnSize::Medium,
            ModalAction::SaveCode(ed.plugin.clone()),
         ) {
            footer.push(btn);
         }
         (
            format!("Edit Plugin · {}", ed.plugin),
            Some("Changes are saved to disk and reloaded immediately.".to_owned()),
            3,
            body,
            footer,
         )
      },
      Snapshot::InstallUrl(inst) => {
         let mut body = gpui::v_stack(10.0);
         if let Some(ref mut b) = body {
            gpui::style_div(b, |s| s.length(s.at.size[0], W_FULL));
            if let Some(tf) = FIELD_SYM.get()
               && let Some(ent) = unsafe {
                  ensure_field_entity(app, window, false, "https://example.com/plugin.js")
               }
            {
               if let Some(init) = &inst.pending_init {
                  unsafe { set_field_text(app, &ent, init) };
               }
               // SAFETY: constructs hosted `TextInput` from `&Entity<FieldState>`
               // in a buffer that fits it and converts to `AnyElement`.
               let mut ti_any = unsafe {
                  let mut ti = RawTextInput::zeroed();
                  (tf.text_input_new)(&raw mut ti, &raw const ent);
                  let mut any = MaybeUninit::<RawAnyElement>::uninit();
                  (tf.text_input_into_any)(any.as_mut_ptr(), &raw mut ti);
                  any.assume_init()
               };
               if let Some(mut wrap) = gpui::v_stack(4.0) {
                  gpui::style_div(&mut wrap, |s| s.length(s.at.size[0], W_FULL));
                  if gpui::push_any(&mut wrap, &mut ti_any) {
                     *b = gpui::push_div(std::mem::replace(b, gpui::raw_div().unwrap()), wrap);
                  }
               }
            } else if let Some(msg) = plain_text_div(
               "Native TextInput could not be resolved on this Discord build.",
               COL_ERROR,
               13.5,
            ) {
               *b = gpui::push_div(std::mem::replace(b, gpui::raw_div().unwrap()), msg);
            }
            if let Some((msg, is_err)) = &inst.status
               && let Some(st) = plain_text_div(msg, if *is_err { COL_ERROR } else { COL_OK }, 13.0)
            {
               *b = gpui::push_div(std::mem::replace(b, gpui::raw_div().unwrap()), st);
            }
         }
         let mut footer = Vec::new();
         if let Some(btn) = make_modal_button(
            "sunbird-url-cancel",
            "Cancel",
            BtnVariant::Secondary,
            BtnSize::Medium,
            ModalAction::Close,
         ) {
            footer.push(btn);
         }
         if !inst.busy
            && let Some(btn) = make_modal_button(
               "sunbird-url-install",
               "Install Plugin",
               BtnVariant::Primary,
               BtnSize::Medium,
               ModalAction::SubmitInstallUrl,
            )
         {
            footer.push(btn);
         }
         (
            "Install Plugin from URL".to_owned(),
            Some("Paste an https:// URL pointing to a Sunbird .js plugin bundle.".to_owned()),
            2,
            body,
            footer,
         )
      },
   };

   let Some(mut body_div) = body_opt else {
      return;
   };
   let Some(close_rc) = gpui::alloc_rc_u64(0) else {
      return;
   };

   // Build `mana_html::hosted::modal::Modal`.
   let at = &msym.layout;
   let title_ss = SharedString::from_str(&title);
   let mut m = RawModal::zeroed();
   // SAFETY: `Modal::new` builds into a buffer that fits a `Modal`.
   unsafe { (msym.modal_new)(&raw mut m, &raw const title_ss) };
   m.put(at.scale, size_code);
   if let Some(sub) = subtitle {
      m.put(at.subtitle, SharedString::from_str(&sub));
   }
   m.put(at.on_close, close_rc);
   m.put(at.on_close + 8, &raw const MODAL_CLOSE_VTBL);
   // SAFETY: `modal_child_div`, `modal_footer_button` and `modal_into_any`
   // take the `Modal` by value and return it through buffers that fit it.
   let mut modal_any = unsafe {
      let mut next = RawModal::zeroed();
      (msym.modal_child_div)(&raw mut next, &raw mut m, &raw mut body_div);
      let mut m = next;

      for mut btn in footer_buttons {
         let mut next = RawModal::zeroed();
         (msym.modal_footer_button)(&raw mut next, &raw mut m, &raw mut btn);
         m = next;
      }

      let mut any = MaybeUninit::<RawAnyElement>::uninit();
      (msym.modal_into_any)(any.as_mut_ptr(), &raw mut m);
      any.assume_init()
   };

   // Build full-window scrim overlay (`position: absolute; inset: 0; flex;
   // center; bg rgba(0,0,0,0.68)`).
   let Some(mut scrim) = gpui::raw_div() else {
      return;
   };
   gpui::style_div(&mut scrim, |s| {
      s.set(s.at.position, POSITION_ABSOLUTE);
      s.lengths(&s.at.inset, gpui_px(0.0));
      s.lengths(&s.at.size, W_FULL);
      s.set(s.at.display, DISPLAY_FLEX);
      s.set(s.at.align_items, ALIGN_CENTER);
      s.set(s.at.justify_content, ALIGN_CENTER);
      s.solid_background([0.0, 0.0, 0.0, 0.68]);
   });
   // The card wraps the modal at its own size, so a press inside it can be told
   // apart from one on the scrim around it.
   let Some(mut card) = gpui::raw_div() else {
      return;
   };
   gpui::style_div(&mut card, |s| s.set(s.at.display, DISPLAY_FLEX));
   if !gpui::push_any(&mut card, &mut modal_any) {
      return;
   }
   on_mouse_down(&mut scrim, &SCRIM_PRESS_VTBL);
   on_mouse_down(&mut card, &CARD_PRESS_VTBL);
   scrim = gpui::push_div(scrim, card);

   // Append the full-screen scrim overlay directly to Discord's
   // `Vec<AnyElement>` overlay stack so it renders on top of chat, Settings,
   // and any Discord modal without needing `LayerHost` to be open.
   let mut scrim_any = MaybeUninit::<RawAnyElement>::uninit();
   gpui::write_any(scrim, scrim_any.as_mut_ptr());
   // SAFETY: `scrim_any` was initialized by `div_into_any` and `vec_push_any`
   // is Discord's own `Vec::<AnyElement>::push`.
   unsafe {
      let mut scrim_any = scrim_any.assume_init();
      (msym.vec_push_any)(overlays, &raw mut scrim_any);
   }
}

#[cfg(test)]
mod tests {
   #[test]
   fn coalesce_folds_whitespace_and_same_colors() {
      let tokens = [
         (1, "  "),
         (2, "\"k\""),
         (3, ":"),
         (1, " "),
         (4, "1"),
         (4, "2"),
         (3, ","),
      ];
      assert_eq!(super::coalesce(tokens.into_iter()), vec![
         (2, "  \"k\"".to_owned()),
         (3, ": ".to_owned()),
         (4, "12".to_owned()),
         (3, ",".to_owned()),
      ]);
   }
}
