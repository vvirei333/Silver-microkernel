//! Wolfser: WerewolfOS's network browser.
//!
//! The program owns no compositor: it writes one RGBA surface into an
//! ivshmem BAR that the host viewer maps from the same file, and publishes
//! frames through `frame_seq`. The address bar accepts a URL or a search
//! query; HTML is reduced to readable styled text (scripts, styles and tags
//! stripped, entities decoded, block tags become paragraph breaks).
//!
//! Plain `http://` is fetched by `net_server` over smoltcp. `https://` (and
//! any redirect that needs it) goes through the host viewer's Qt proxy: the
//! guest publishes the URL in the shared header, the host fetches it with TLS
//! and writes the body back. Both paths feed the same renderer.
//!
//! Links are extracted with their screen runs, so a left click on blue text
//! navigates; back/forward keep a URL history.

#![no_std]
#![no_main]

#[path = "../font5x7.rs"]
mod font5x7;
#[allow(dead_code)]
#[path = "../ipc.rs"]
mod ipc;
#[allow(dead_code)]
#[path = "../net_ipc.rs"]
mod net_ipc;
#[path = "../syscall.rs"]
mod syscall;
#[path = "../window_proto.rs"]
mod window;

use core::panic::PanicInfo;

const WIDTH: u32 = 900;
const HEIGHT: u32 = 640;
const STRIDE: u32 = WIDTH * 4;
const HEADER_HEIGHT: u32 = 78;
const FOOTER_HEIGHT: u32 = 16;
const MARGIN: u32 = 12;
const ADDRESS_X: u32 = 130;
const ADDRESS_Y: u32 = 29;
const ADDRESS_WIDTH: u32 = WIDTH - ADDRESS_X - 8;
const ADDRESS_HEIGHT: u32 = 24;
const ADDRESS_TEXT_Y: u32 = 37;
const BODY_TOP: u32 = HEADER_HEIGHT + 8;
const SCALE: u32 = 2;
const CELL_WIDTH: u32 = 12;
const LINE_HEIGHT: u32 = 18;
const HEADING_SCALE: u32 = 3;
const HEADING_CELL: u32 = 18;
const HEADING_LINE_HEIGHT: u32 = 25;
const TEXT_COLUMNS: u32 = (WIDTH - 2 * MARGIN) / CELL_WIDTH;
const HEADING_COLUMNS: u32 = (WIDTH - 2 * MARGIN) / HEADING_CELL;

const BUTTON_WIDTH: u32 = 26;
const BUTTON_HEIGHT: u32 = 24;
const BUTTON_Y: u32 = 29;
const BUTTON_BACK_X: u32 = 8;
const BUTTON_FORWARD_X: u32 = 38;
const BUTTON_RELOAD_X: u32 = 68;
const BUTTON_HOME_X: u32 = 98;

const COLOR_BACKGROUND: u32 = 0xFFFF_FFFF;
const COLOR_HEADER: u32 = 0xFF5F_3A1E;
const COLOR_HEADER_TEXT: u32 = 0xFFFF_FFFF;
const COLOR_HEADER_TITLE: u32 = 0xFFECDDD0;
const COLOR_ADDRESS: u32 = 0xFFFF_FFFF;
const COLOR_ADDRESS_BORDER: u32 = 0xFF3A_2412;
const COLOR_TEXT: u32 = 0xFF20_2020;
const COLOR_HEADING: u32 = 0xFF10_1010;
const COLOR_LINK: u32 = 0xFF1A_4FA0;
const COLOR_PLACEHOLDER: u32 = 0xFF90_9090;
const COLOR_MUTED: u32 = 0xFF60_6060;
const COLOR_CODE: u32 = 0xFF33_3333;
const COLOR_FOOTER: u32 = 0xFFF2_EEE8;
const COLOR_BUTTON: u32 = 0xFF7A_4A28;
const COLOR_TAB_ACTIVE: u32 = 0xFF7A_4A28;
const COLOR_TAB: u32 = 0xFF47_2A15;
const COLOR_TAB_TEXT_ACTIVE: u32 = 0xFFFF_FFFF;
const COLOR_TAB_TEXT: u32 = 0xFFD8_C3B2;
const COLOR_MATCH: u32 = 0xFFFF_E08A;
const COLOR_MATCH_CURRENT: u32 = 0xFFFF_9A3A;
const COLOR_FIND: u32 = 0xFFFF_F3C4;
const COLOR_TILE: u32 = 0xFFF4_EFE9;
const COLOR_TILE_BORDER: u32 = 0xFFD8_C7B6;

const TAB_MAX: usize = 4;
const TAB_HISTORY: usize = 12;
const TAB_TITLE: usize = 80;
const TAB_START_X: u32 = 6;
const TAB_WIDTH: u32 = 148;
const TAB_GAP: u32 = 3;
const TAB_Y: u32 = 3;
const TAB_BAR_HEIGHT: u32 = 22;
const TAB_TEXT_Y: u32 = 9;
const TAB_CLOSE_WIDTH: u32 = 18;
const FIND_CAPACITY: usize = 64;
const BOOKMARK_MAX: usize = 8;
const BOOKMARK_TITLE: usize = 80;
const TOAST_TICKS: u64 = 2 * TICKS_PER_SECOND;

const TILE_X: u32 = 24;
const TILE_Y: u32 = BODY_TOP + 64;
const TILE_WIDTH: u32 = 273;
const TILE_HEIGHT: u32 = 84;
const TILE_GAP: u32 = 16;
const BOOKMARK_Y: u32 = BODY_TOP + 196;
const BOOKMARK_ROW: u32 = 22;

const QUICK_LINKS: [(&str, &str); 3] = [
    ("SEARCH THE WEB", ""),
    ("ENCYCLOPEDIA", "https://en.wikipedia.org/"),
    ("TEST PAGE", "https://example.com/"),
];
const QUICK_HINTS: [&str; 3] = ["DUCKDUCKGO", "WIKIPEDIA.ORG", "EXAMPLE.COM"];

const DEFAULT_SEARCH: &str = "https://html.duckduckgo.com/html/?q=";
const IVSHMEM_VENDOR: u16 = 0x1AF4;
const IVSHMEM_DEVICE: u16 = 0x1110;
const IVSHMEM_BAR: u64 = 0x1040_0000;
const WINDOW_MAX_SIZE: u64 = 8 * 1024 * 1024;

const TEXT_CAPACITY: usize = 40 * 1024;
const TITLE_CAPACITY: usize = 160;
const ADDRESS_CAPACITY: usize = 512;
const URL_CAPACITY: usize = 512;
const MAX_LINK_RUNS: usize = 256;
const LINK_POOL: usize = 24 * 1024;
const MAX_REDIRECTS: u8 = 8;

const TICKS_PER_SECOND: u64 = 100;
const FETCH_TIMEOUT_TICKS: u64 = 35 * TICKS_PER_SECOND;
const RESPONSE_POLL_TICKS: u64 = 2;

// Per-character style flags.
const STYLE_LINK: u8 = 0x01;
const STYLE_HEADING: u8 = 0x02;
const STYLE_CODE: u8 = 0x04;

struct App {
    window: u64,
    server: u64,
    net_base: u64,
    address: [u8; ADDRESS_CAPACITY],
    address_len: usize,
    focused: bool,
    active: usize,
    scroll: usize,
    hover: [u8; URL_CAPACITY],
    hover_len: usize,
    image_pending: bool,
    image_seq: u32,
    image_started: u64,
    image_ready: bool,
    image_w: u32,
    image_h: u32,
    find_mode: bool,
    find_query: [u8; FIND_CAPACITY],
    find_len: usize,
    find_total: usize,
    find_index: usize,
    toast_until: u64,
    toast: &'static str,
    page: PageState,
    pending: Option<Pending>,
}

struct PageState {
    loaded: bool,
    loading: bool,
    status: u16,
    error: &'static str,
}

struct Pending {
    backend: Backend,
    url: [u8; URL_CAPACITY],
    url_len: usize,
    started: u64,
    hops: u8,
    request_id: u32,
    host_seq: u32,
    from_history: bool,
    retries: u8,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Backend {
    Guest,
    Host,
}

#[derive(Copy, Clone)]
struct LinkRun {
    url_off: u32,
    url_len: u16,
    line: u16,
    start: u16,
    end: u16,
}

/// One tab owns its URL, title, scroll position, history and a cache of the
/// extracted page (text, styles, link runs and link targets). Only the active
/// tab is mirrored into the global render buffers; switching copies the cache
/// back, so tabs do not refetch.
#[derive(Copy, Clone)]
struct Tab {
    open: bool,
    url: [u8; URL_CAPACITY],
    url_len: usize,
    address: [u8; ADDRESS_CAPACITY],
    address_len: usize,
    title: [u8; TAB_TITLE],
    title_len: usize,
    scroll: usize,
    loaded: bool,
    status: u16,
    error: &'static str,
    history: [[u8; URL_CAPACITY]; TAB_HISTORY],
    history_lens: [u16; TAB_HISTORY],
    history_len: usize,
    history_pos: usize,
    text: [u8; TEXT_CAPACITY],
    text_len: usize,
    style: [u8; TEXT_CAPACITY],
    link_runs: [LinkRun; MAX_LINK_RUNS],
    link_count: usize,
    link_pool: [u8; LINK_POOL],
    link_pool_len: usize,
    image_url: [u8; URL_CAPACITY],
    image_len: usize,
}

impl Tab {
    const EMPTY: Tab = Tab {
        open: false,
        url: [0; URL_CAPACITY],
        url_len: 0,
        address: [0; ADDRESS_CAPACITY],
        address_len: 0,
        title: [0; TAB_TITLE],
        title_len: 0,
        scroll: 0,
        loaded: false,
        status: 0,
        error: "",
        history: [[0; URL_CAPACITY]; TAB_HISTORY],
        history_lens: [0; TAB_HISTORY],
        history_len: 0,
        history_pos: 0,
        text: [0; TEXT_CAPACITY],
        text_len: 0,
        style: [0; TEXT_CAPACITY],
        link_runs: [LinkRun {
            url_off: 0,
            url_len: 0,
            line: 0,
            start: 0,
            end: 0,
        }; MAX_LINK_RUNS],
        link_count: 0,
        link_pool: [0; LINK_POOL],
        link_pool_len: 0,
        image_url: [0; URL_CAPACITY],
        image_len: 0,
    };

    fn title_text(&self) -> &str {
        core::str::from_utf8(&self.title[..self.title_len]).unwrap_or("")
    }
}

#[derive(Copy, Clone)]
struct Bookmark {
    title: [u8; BOOKMARK_TITLE],
    title_len: usize,
    url: [u8; URL_CAPACITY],
    url_len: usize,
}

impl Bookmark {
    const EMPTY: Bookmark = Bookmark {
        title: [0; BOOKMARK_TITLE],
        title_len: 0,
        url: [0; URL_CAPACITY],
        url_len: 0,
    };
}

static mut BOOKMARKS: [Bookmark; BOOKMARK_MAX] = [Bookmark::EMPTY; BOOKMARK_MAX];
static mut BOOKMARK_COUNT: usize = 0;

static mut TABS: [Tab; TAB_MAX] = [Tab::EMPTY; TAB_MAX];

static mut TEXT: [u8; TEXT_CAPACITY] = [0; TEXT_CAPACITY];
static mut STYLE: [u8; TEXT_CAPACITY] = [0; TEXT_CAPACITY];
static mut TEXT_LEN: usize = 0;
static mut TITLE: [u8; TITLE_CAPACITY] = [0; TITLE_CAPACITY];
static mut TITLE_LEN: usize = 0;
static mut LINK_RUNS: [LinkRun; MAX_LINK_RUNS] = [LinkRun {
    url_off: 0,
    url_len: 0,
    line: 0,
    start: 0,
    end: 0,
}; MAX_LINK_RUNS];
static mut LINK_RUN_COUNT: usize = 0;
static mut LINK_POOL_BYTES: [u8; LINK_POOL] = [0; LINK_POOL];
static mut LINK_POOL_LEN: usize = 0;

static mut REQUEST_ID: u32 = 0;
static mut PAGE_REDIRECT: [u8; URL_CAPACITY] = [0; URL_CAPACITY];
static mut PAGE_REDIRECT_LEN: usize = 0;

// Large navigation buffers are statics, not locals: a user task has an 8 KiB
// stack, and the redirect chain (page -> Location -> resolved URL) used to
// overflow it exactly when a clicked link redirected through two hops.
static mut RESOLVE_BUFFER: [u8; URL_CAPACITY] = [0; URL_CAPACITY];
static mut NORMALIZE_BUFFER: [u8; URL_CAPACITY] = [0; URL_CAPACITY];
static mut REDIRECT_HREF: [u8; URL_CAPACITY] = [0; URL_CAPACITY];
static mut REDIRECT_TARGET: [u8; URL_CAPACITY] = [0; URL_CAPACITY];
static mut HOST_FINAL: [u8; URL_CAPACITY] = [0; URL_CAPACITY];
static mut BASE_URL: [u8; URL_CAPACITY] = [0; URL_CAPACITY];
static mut CLICK_HREF: [u8; URL_CAPACITY] = [0; URL_CAPACITY];
static mut CLICK_TARGET: [u8; URL_CAPACITY] = [0; URL_CAPACITY];
static mut IMAGE_URL: [u8; URL_CAPACITY] = [0; URL_CAPACITY];
static mut IMAGE_URL_LEN: usize = 0;
static mut IMAGE_HREF: [u8; URL_CAPACITY] = [0; URL_CAPACITY];

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argument: u64) -> ! {
    let mut arguments = [0_u8; 256];
    let length = syscall::arguments(&mut arguments).unwrap_or(0);
    let initial = core::str::from_utf8(&arguments[..length])
        .unwrap_or("")
        .trim();

    let server = find_task_id(syscall::PROGRAM_NET_SERVER).unwrap_or(0);
    let window_base = match open_window() {
        Ok(base) => base,
        Err(stage) => fatal(
            20 + stage,
            "NO IVSHMEM WINDOW (START QEMU WITH -device ivshmem-plain)",
        ),
    };
    let net_base = if server != 0 {
        map_net_window(server).unwrap_or(0)
    } else {
        0
    };

    let mut app = App {
        window: window_base,
        server,
        net_base,
        address: [0; ADDRESS_CAPACITY],
        address_len: 0,
        focused: true,
        active: 0,
        scroll: 0,
        hover: [0; URL_CAPACITY],
        hover_len: 0,
        image_pending: false,
        image_seq: 0,
        image_started: 0,
        image_ready: false,
        image_w: 0,
        image_h: 0,
        find_mode: false,
        find_query: [0; FIND_CAPACITY],
        find_len: 0,
        find_total: 0,
        find_index: 0,
        toast_until: 0,
        toast: "",
        page: PageState {
            loaded: false,
            loading: false,
            status: 0,
            error: "",
        },
        pending: None,
    };

    unsafe {
        let tabs = &mut *core::ptr::addr_of_mut!(TABS);
        tabs[0] = Tab::EMPTY;
        tabs[0].open = true;
    }

    if !initial.is_empty() {
        set_address(&mut app, initial);
        app.focused = false;
        let mut url = [0_u8; URL_CAPACITY];
        if let Some(len) = resolve_url(initial, &mut url) {
            let target = core::str::from_utf8(&url[..len]).unwrap_or("");
            begin_navigation(&mut app, target, false, false);
        }
    }
    paint(&app);

    loop {
        let head = window::read_u32(app.window, window::FIELD_INPUT_HEAD);
        let mut tail = window::read_u32(app.window, window::FIELD_INPUT_TAIL);
        let mut dirty = false;
        while tail != head {
            let event = window::read_input(app.window, tail);
            tail = (tail + 1) % window::INPUT_SLOTS;
            match event.kind {
                window::INPUT_KEY => {
                    if handle_key(&mut app, event.key) {
                        dirty = true;
                    }
                }
                window::INPUT_SCROLL => {
                    let delta = event.value as i8;
                    if delta >= 0 {
                        app.scroll = app.scroll.saturating_add(delta as usize);
                    } else {
                        app.scroll = app.scroll.saturating_sub((-delta) as usize);
                    }
                    dirty = true;
                }
                window::INPUT_CLICK => {
                    if handle_click(
                        &mut app,
                        event.x as u32,
                        event.y as u32,
                        event.value,
                    ) {
                        dirty = true;
                    }
                }
                window::INPUT_MOVE => {
                    if handle_move(
                        &mut app,
                        event.x as u32,
                        event.y as u32,
                        event.value != 0,
                    ) {
                        dirty = true;
                    }
                }
                _ => {}
            }
        }
        if tail != window::read_u32(app.window, window::FIELD_INPUT_TAIL) {
            window::write_u32(app.window, window::FIELD_INPUT_TAIL, tail);
        }

        if app.pending.is_some() {
            poll_pending(&mut app);
            // The spinner keeps moving, so repaint even without input.
            dirty = true;
        }
        if app.image_pending && poll_image(&mut app) {
            dirty = true;
        }
        if !app.toast.is_empty() && syscall::uptime_ticks() >= app.toast_until {
            app.toast = "";
            dirty = true;
        }

        if dirty {
            clamp_scroll(&mut app);
            paint(&app);
        }
        if app.pending.is_some() {
            syscall::sleep(80);
        } else {
            syscall::sleep(40);
        }
    }
}

fn clamp_scroll(app: &mut App) {
    let lines = line_count() as usize;
    let max = lines.saturating_sub(1);
    if app.scroll > max {
        app.scroll = max;
    }
}

/// Returns true when the key changed something visible.
fn handle_key(app: &mut App, key: u16) -> bool {
    if key == window::KEY_FIND {
        if app.find_mode {
            app.find_mode = false;
        } else {
            app.find_mode = true;
            app.find_len = 0;
            app.find_total = 0;
            app.find_index = 0;
            app.focused = false;
        }
        return true;
    }
    if key == window::KEY_QUIT {
        syscall::exit();
    }
    if key == window::KEY_FOCUS {
        app.find_mode = false;
        app.focused = true;
        app.address_len = 0;
        app.address[0] = 0;
        return true;
    }
    if app.find_mode {
        return handle_find_key(app, key);
    }
    match key {
        window::KEY_ENTER => {
            if app.focused {
                submit(app);
                true
            } else {
                false
            }
        }
        window::KEY_BACKSPACE => {
            if app.focused && app.address_len > 0 {
                app.address_len -= 1;
                app.address[app.address_len] = 0;
                true
            } else if app.focused {
                false
            } else {
                go_history(app, -1);
                true
            }
        }
        window::KEY_ESCAPE => {
            if app.pending.is_some() {
                cancel_pending(app);
                true
            } else if app.focused {
                app.focused = false;
                true
            } else {
                false
            }
        }
        window::KEY_BACK => {
            go_history(app, -1);
            true
        }
        window::KEY_FORWARD => {
            go_history(app, 1);
            true
        }
        window::KEY_RELOAD => {
            reload(app);
            true
        }
        window::KEY_NEW_TAB => {
            new_tab(app);
            true
        }
        window::KEY_CLOSE_TAB => {
            close_tab(app, app.active);
            true
        }
        window::KEY_NEXT_TAB => {
            next_tab(app);
            true
        }
        window::KEY_HOME_PAGE => {
            show_start(app);
            true
        }
        window::KEY_BOOKMARK => {
            bookmark_current(app);
            true
        }
        window::KEY_UP => {
            app.scroll = app.scroll.saturating_sub(1);
            true
        }
        window::KEY_DOWN => {
            app.scroll = app.scroll.saturating_add(1);
            true
        }
        window::KEY_PAGE_UP => {
            app.scroll = app.scroll.saturating_sub(22);
            true
        }
        window::KEY_PAGE_DOWN => {
            app.scroll = app.scroll.saturating_add(22);
            true
        }
        window::KEY_HOME => {
            app.scroll = 0;
            true
        }
        window::KEY_END => {
            app.scroll = line_count().saturating_sub(1) as usize;
            true
        }
        key if (0x20..0x7F).contains(&key) => {
            if !app.focused {
                // Typing anywhere in the page starts a new address/search,
                // exactly like a normal browser.
                app.focused = true;
                app.address_len = 0;
                app.address[0] = 0;
            }
            if app.address_len < app.address.len() - 1 {
                app.address[app.address_len] = key as u8;
                app.address_len += 1;
                app.address[app.address_len] = 0;
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

fn handle_find_key(app: &mut App, key: u16) -> bool {
    match key {
        window::KEY_ENTER => {
            if app.find_total > 0 {
                app.find_index = (app.find_index + 1) % app.find_total;
                focus_match(app, app.find_index);
            }
            true
        }
        window::KEY_ESCAPE => {
            app.find_mode = false;
            true
        }
        window::KEY_BACKSPACE => {
            if app.find_len > 0 {
                app.find_len -= 1;
                refresh_find(app);
                true
            } else {
                false
            }
        }
        key if (0x20..0x7F).contains(&key) => {
            if app.find_len + 1 < app.find_query.len() {
                app.find_query[app.find_len] = key as u8;
                app.find_len += 1;
                refresh_find(app);
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

fn find_needle(app: &App) -> &[u8] {
    &app.find_query[..app.find_len]
}

fn count_matches(line: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || needle.len() > line.len() {
        return 0;
    }
    let mut count = 0usize;
    let mut index = 0usize;
    while index + needle.len() <= line.len() {
        if line[index..index + needle.len()].eq_ignore_ascii_case(needle) {
            count += 1;
            index += needle.len();
        } else {
            index += 1;
        }
    }
    count
}

fn refresh_find(app: &mut App) {
    app.find_total = 0;
    app.find_index = 0;
    if app.find_len == 0 {
        return;
    }
    let text = unsafe { &*core::ptr::addr_of!(TEXT) };
    let length = unsafe { *core::ptr::addr_of!(TEXT_LEN) };
    let needle = &app.find_query[..app.find_len];
    let mut start = 0usize;
    while start <= length {
        let mut end = start;
        while end < length && text[end] != b'\n' {
            end += 1;
        }
        app.find_total += count_matches(&text[start..end], needle);
        if end >= length {
            break;
        }
        start = end + 1;
    }
    if app.find_total > 0 {
        focus_match(app, 0);
    }
}

fn focus_match(app: &mut App, target: usize) {
    if app.find_len == 0 {
        return;
    }
    let text = unsafe { &*core::ptr::addr_of!(TEXT) };
    let length = unsafe { *core::ptr::addr_of!(TEXT_LEN) };
    let needle = &app.find_query[..app.find_len];
    let mut seen = 0usize;
    let mut line = 0usize;
    let mut start = 0usize;
    while start <= length {
        let mut end = start;
        while end < length && text[end] != b'\n' {
            end += 1;
        }
        let count = count_matches(&text[start..end], needle);
        if target < seen + count {
            app.scroll = line.saturating_sub(1);
            return;
        }
        seen += count;
        line += 1;
        if end >= length {
            break;
        }
        start = end + 1;
    }
}

fn handle_click(app: &mut App, x: u32, y: u32, button: u8) -> bool {
    if (TAB_Y..TAB_Y + TAB_BAR_HEIGHT).contains(&y) {
        let count = open_tab_count();
        let mut cursor = TAB_START_X;
        for screen in 0..count {
            if x >= cursor && x < cursor + TAB_WIDTH {
                let Some(slot) = open_slot_at(screen) else {
                    return false;
                };
                if x >= cursor + TAB_WIDTH - TAB_CLOSE_WIDTH {
                    close_tab(app, slot);
                } else {
                    switch_tab(app, slot);
                }
                return true;
            }
            cursor += TAB_WIDTH + TAB_GAP;
        }
        if x >= cursor && x < cursor + TAB_BAR_HEIGHT {
            new_tab(app);
            return true;
        }
        return false;
    }
    if (BUTTON_Y..BUTTON_Y + BUTTON_HEIGHT).contains(&y) {
        if (BUTTON_BACK_X..BUTTON_BACK_X + BUTTON_WIDTH).contains(&x) {
            go_history(app, -1);
            return true;
        }
        if (BUTTON_FORWARD_X..BUTTON_FORWARD_X + BUTTON_WIDTH).contains(&x) {
            go_history(app, 1);
            return true;
        }
        if (BUTTON_RELOAD_X..BUTTON_RELOAD_X + BUTTON_WIDTH).contains(&x) {
            reload(app);
            return true;
        }
        if (BUTTON_HOME_X..BUTTON_HOME_X + BUTTON_WIDTH).contains(&x) {
            show_start(app);
            return true;
        }
    }
    if (ADDRESS_Y..ADDRESS_Y + ADDRESS_HEIGHT).contains(&y)
        && x >= ADDRESS_X
        && x < ADDRESS_X + ADDRESS_WIDTH
    {
        app.find_mode = false;
        app.focused = true;
        return true;
    }
    if y >= BODY_TOP && y < HEIGHT - FOOTER_HEIGHT {
        if !app.page.loaded {
            if let Some(action) = start_action_at(x, y) {
                execute_start_action(app, action);
                return true;
            }
            return false;
        }
        if let Some((offset, length)) = hit_link(app, x, y) {
            let href = unsafe { &mut *core::ptr::addr_of_mut!(CLICK_HREF) };
            let count = length as usize;
            unsafe {
                let pool = &*core::ptr::addr_of!(LINK_POOL_BYTES);
                if offset as usize + count <= pool.len() {
                    href[..count].copy_from_slice(&pool[offset as usize..offset as usize + count]);
                }
            }
            let target = unsafe { &mut *core::ptr::addr_of_mut!(CLICK_TARGET) };
            let base = {
                let tab = tab_ref(app.active);
                core::str::from_utf8(&tab.url[..tab.url_len]).unwrap_or("")
            };
            let text = core::str::from_utf8(&href[..count]).unwrap_or("");
            if let Some(len) = resolve_link(base, text, target) {
                let url = core::str::from_utf8(&target[..len]).unwrap_or("");
                if button == 2 || button == 3 {
                    new_tab(app);
                }
                begin_navigation(app, url, false, false);
                return true;
            }
        }
    }
    false
}

fn apply_hover(app: &mut App, text: &str) -> bool {
    let length = text.len().min(URL_CAPACITY);
    let changed = length != app.hover_len
        || (length > 0 && app.hover[..length] != text.as_bytes()[..length]);
    if changed {
        app.hover_len = length;
        if length > 0 {
            app.hover[..length].copy_from_slice(&text.as_bytes()[..length]);
        }
        let cursor = u32::from(length > 0);
        window::write_u32(app.window, window::FIELD_CURSOR, cursor);
    }
    changed
}

fn handle_move(app: &mut App, x: u32, y: u32, leave: bool) -> bool {
    if leave {
        return apply_hover(app, "");
    }
    if !app.page.loaded {
        let text = match start_action_at(x, y) {
            Some(StartAction::Search) => Some("SEARCH THE WEB, THEN PRESS ENTER"),
            Some(StartAction::Url(url)) => Some(url),
            Some(StartAction::Bookmark(index)) => {
                let source = unsafe { &mut *core::ptr::addr_of_mut!(BASE_URL) };
                let length = unsafe {
                    let bookmarks = &*core::ptr::addr_of!(BOOKMARKS);
                    let length = bookmarks[index].url_len.min(URL_CAPACITY);
                    source[..length].copy_from_slice(&bookmarks[index].url[..length]);
                    length
                };
                let source = unsafe { &*core::ptr::addr_of!(BASE_URL) };
                Some(core::str::from_utf8(&source[..length]).unwrap_or(""))
            }
            None => None,
        };
        return apply_hover(app, text.unwrap_or(""));
    }
    let mut url = [0_u8; URL_CAPACITY];
    let mut length = 0usize;
    if !leave {
        if let Some((offset, count)) = hit_link(app, x, y) {
            let count = count as usize;
            let href = unsafe { &mut *core::ptr::addr_of_mut!(CLICK_HREF) };
            unsafe {
                let pool = &*core::ptr::addr_of!(LINK_POOL_BYTES);
                if offset as usize + count <= pool.len() {
                    href[..count].copy_from_slice(&pool[offset as usize..offset as usize + count]);
                }
            }
            let target = unsafe { &mut *core::ptr::addr_of_mut!(CLICK_TARGET) };
            let base = {
                let tab = tab_ref(app.active);
                core::str::from_utf8(&tab.url[..tab.url_len]).unwrap_or("")
            };
            let text = core::str::from_utf8(&href[..count]).unwrap_or("");
            if let Some(len) = resolve_link(base, text, target) {
                url[..len].copy_from_slice(&target[..len]);
                length = len;
            }
        }
    }
    apply_hover(app, core::str::from_utf8(&url[..length]).unwrap_or(""))
}

fn hit_link(app: &App, x: u32, y: u32) -> Option<(u32, u16)> {
    let text = unsafe { &*core::ptr::addr_of!(TEXT) };
    let styles = unsafe { &*core::ptr::addr_of!(STYLE) };
    let length = unsafe { *core::ptr::addr_of!(TEXT_LEN) };
    let runs = unsafe { &*core::ptr::addr_of!(LINK_RUNS) };
    let run_count = unsafe { *core::ptr::addr_of!(LINK_RUN_COUNT) };
    let scroll_pixels = pixels_before(app.scroll);
    let mut top = BODY_TOP as i32 + image_height(app) as i32 - scroll_pixels as i32;
    let mut line = 0u32;
    let mut start = 0usize;
    while start <= length {
        let mut end = start;
        while end < length && text[end] != b'\n' {
            end += 1;
        }
        let flags = if start < end { styles[start] } else { 0 };
        let height = line_height(flags);
        if y as i32 >= top && (y as i32) < top + height as i32 {
            let cell = if flags & STYLE_HEADING != 0 {
                HEADING_CELL
            } else {
                CELL_WIDTH
            };
            if x >= MARGIN {
                let column = (x - MARGIN) / cell;
                for run in &runs[..run_count] {
                    if u32::from(run.line) == line
                        && column >= u32::from(run.start)
                        && column < u32::from(run.end)
                    {
                        return Some((run.url_off, run.url_len));
                    }
                }
            }
            return None;
        }
        top += height as i32;
        line += 1;
        if end >= length {
            break;
        }
        start = end + 1;
    }
    None
}

fn set_address(app: &mut App, text: &str) {
    let length = text.len().min(app.address.len() - 1);
    app.address[..length].copy_from_slice(&text.as_bytes()[..length]);
    app.address_len = length;
    app.address[length] = 0;
}


impl App {
    fn address_text(&self) -> &str {
        core::str::from_utf8(&self.address[..self.address_len]).unwrap_or("")
    }
}

fn tabs() -> &'static mut [Tab; TAB_MAX] {
    unsafe { &mut *core::ptr::addr_of_mut!(TABS) }
}

fn tab_ref(slot: usize) -> &'static Tab {
    unsafe { &(*core::ptr::addr_of!(TABS))[slot] }
}

fn open_tab_count() -> usize {
    unsafe { (*core::ptr::addr_of!(TABS)).iter().filter(|tab| tab.open).count() }
}

fn open_slot_at(index: usize) -> Option<usize> {
    let mut seen = 0usize;
    unsafe {
        let tabs = &*core::ptr::addr_of!(TABS);
        for (slot, tab) in tabs.iter().enumerate() {
            if tab.open {
                if seen == index {
                    return Some(slot);
                }
                seen += 1;
            }
        }
    }
    None
}

/// Mirrors the live render buffers into the active tab's cache.
fn save_active(app: &App) {
    let tab = &mut tabs()[app.active];
    tab.address_len = app.address_len;
    tab.address[..app.address_len].copy_from_slice(&app.address[..app.address_len]);
    tab.scroll = app.scroll;
    tab.loaded = app.page.loaded;
    tab.status = app.page.status;
    tab.error = app.page.error;

    unsafe {
        let text = &*core::ptr::addr_of!(TEXT);
        let styles = &*core::ptr::addr_of!(STYLE);
        let title = &*core::ptr::addr_of!(TITLE);
        let runs = &*core::ptr::addr_of!(LINK_RUNS);
        let pool = &*core::ptr::addr_of!(LINK_POOL_BYTES);
        let text_len = *core::ptr::addr_of!(TEXT_LEN);
        let title_len = *core::ptr::addr_of!(TITLE_LEN);
        let run_count = *core::ptr::addr_of!(LINK_RUN_COUNT);
        let pool_len = *core::ptr::addr_of!(LINK_POOL_LEN);
        tab.text_len = text_len.min(TEXT_CAPACITY);
        tab.text[..tab.text_len].copy_from_slice(&text[..tab.text_len]);
        tab.style[..tab.text_len].copy_from_slice(&styles[..tab.text_len]);
        tab.title_len = title_len.min(TAB_TITLE);
        tab.title[..tab.title_len].copy_from_slice(&title[..tab.title_len]);
        tab.link_count = run_count.min(MAX_LINK_RUNS);
        tab.link_runs[..tab.link_count].copy_from_slice(&runs[..tab.link_count]);
        tab.link_pool_len = pool_len.min(LINK_POOL);
        tab.link_pool[..tab.link_pool_len].copy_from_slice(&pool[..tab.link_pool_len]);
    }
}

/// Restores a tab's cached page into the live render buffers.
fn restore_active(app: &mut App, slot: usize) {
    app.active = slot;
    let tab = tab_ref(slot);
    app.address_len = tab.address_len.min(ADDRESS_CAPACITY - 1);
    app.address[..app.address_len].copy_from_slice(&tab.address[..app.address_len]);
    app.address[app.address_len] = 0;
    app.scroll = tab.scroll;
    app.page = PageState {
        loaded: tab.loaded,
        loading: false,
        status: tab.status,
        error: tab.error,
    };
    unsafe {
        let text = &mut *core::ptr::addr_of_mut!(TEXT);
        let styles = &mut *core::ptr::addr_of_mut!(STYLE);
        let title = &mut *core::ptr::addr_of_mut!(TITLE);
        let runs = &mut *core::ptr::addr_of_mut!(LINK_RUNS);
        let pool = &mut *core::ptr::addr_of_mut!(LINK_POOL_BYTES);
        let text_len = tab.text_len.min(TEXT_CAPACITY);
        text[..text_len].copy_from_slice(&tab.text[..text_len]);
        styles[..text_len].copy_from_slice(&tab.style[..text_len]);
        *core::ptr::addr_of_mut!(TEXT_LEN) = text_len;
        let title_len = tab.title_len.min(TITLE_CAPACITY);
        title[..title_len].copy_from_slice(&tab.title[..title_len]);
        *core::ptr::addr_of_mut!(TITLE_LEN) = title_len;
        let run_count = tab.link_count.min(MAX_LINK_RUNS);
        runs[..run_count].copy_from_slice(&tab.link_runs[..run_count]);
        *core::ptr::addr_of_mut!(LINK_RUN_COUNT) = run_count;
        let pool_len = tab.link_pool_len.min(LINK_POOL);
        pool[..pool_len].copy_from_slice(&tab.link_pool[..pool_len]);
        *core::ptr::addr_of_mut!(LINK_POOL_LEN) = pool_len;
    }
}

fn reset_active_view(app: &mut App) {
    app.address_len = 0;
    app.address[0] = 0;
    app.focused = true;
    app.scroll = 0;
    app.toast = "";
    app.page = PageState {
        loaded: false,
        loading: false,
        status: 0,
        error: "",
    };
    unsafe {
        *core::ptr::addr_of_mut!(TEXT_LEN) = 0;
        *core::ptr::addr_of_mut!(TITLE_LEN) = 0;
        *core::ptr::addr_of_mut!(LINK_RUN_COUNT) = 0;
        *core::ptr::addr_of_mut!(LINK_POOL_LEN) = 0;
    }
}

fn clear_image(app: &mut App) {
    app.image_pending = false;
    app.image_ready = false;
    app.image_w = 0;
    app.image_h = 0;
}

fn start_image_from_global(app: &mut App) {
    let url = unsafe {
        let image = &*core::ptr::addr_of!(IMAGE_URL);
        let len = *core::ptr::addr_of!(IMAGE_URL_LEN);
        core::str::from_utf8(&image[..len]).unwrap_or("")
    };
    start_image(app, url);
}

fn start_image(app: &mut App, url: &str) {
    clear_image(app);
    if url.is_empty() || window::read_u32(app.window, window::FIELD_WEB_CAPABLE) == 0 {
        return;
    }
    let length = url.len().min(window::IMG_URL_MAX);
    unsafe {
        let base = app.window + window::IMG_URL;
        core::ptr::copy_nonoverlapping(url.as_ptr(), base as *mut u8, length);
    }
    window::write_u32(app.window, window::FIELD_IMG_REQUEST_LEN, length as u32);
    net_ipc::fence();
    let seq = window::read_u32(app.window, window::FIELD_IMG_REQUEST_SEQ).wrapping_add(1);
    window::write_u32(app.window, window::FIELD_IMG_REQUEST_SEQ, seq);
    app.image_seq = seq;
    app.image_started = syscall::uptime_ticks();
    app.image_pending = true;
}

fn poll_image(app: &mut App) -> bool {
    if !app.image_pending {
        return false;
    }
    if syscall::uptime_ticks().saturating_sub(app.image_started) > 15 * TICKS_PER_SECOND {
        app.image_pending = false;
        return true;
    }
    if window::read_u32(app.window, window::FIELD_IMG_DONE_SEQ) != app.image_seq {
        return false;
    }
    let width = window::read_u32(app.window, window::FIELD_IMG_WIDTH);
    let height = window::read_u32(app.window, window::FIELD_IMG_HEIGHT);
    let length = window::read_u32(app.window, window::FIELD_IMG_LEN);
    let error = window::read_u32(app.window, window::FIELD_IMG_ERROR);
    app.image_pending = false;
    if error == 0
        && width >= 160
        && height >= 100
        && width <= 480
        && height <= 360
        && length == width.saturating_mul(height).saturating_mul(4)
    {
        app.image_ready = true;
        app.image_w = width;
        app.image_h = height;
    }
    true
}

fn image_height(app: &App) -> u32 {
    if app.image_ready {
        app.image_h + 10
    } else {
        0
    }
}

#[derive(Copy, Clone)]
enum StartAction {
    Search,
    Url(&'static str),
    Bookmark(usize),
}

fn start_action_at(x: u32, y: u32) -> Option<StartAction> {
    if y >= TILE_Y && y < TILE_Y + TILE_HEIGHT {
        for index in 0..QUICK_LINKS.len() {
            let left = TILE_X + index as u32 * (TILE_WIDTH + TILE_GAP);
            if x >= left && x < left + TILE_WIDTH {
                let (_, url) = QUICK_LINKS[index];
                return Some(if url.is_empty() {
                    StartAction::Search
                } else {
                    StartAction::Url(url)
                });
            }
        }
        return None;
    }
    let count = unsafe { *core::ptr::addr_of!(BOOKMARK_COUNT) };
    if count > 0 && y >= BOOKMARK_Y && y < BOOKMARK_Y + count as u32 * BOOKMARK_ROW {
        let index = ((y - BOOKMARK_Y) / BOOKMARK_ROW) as usize;
        if index < count && x >= 24 && x < WIDTH - 24 {
            return Some(StartAction::Bookmark(index));
        }
    }
    None
}

fn execute_start_action(app: &mut App, action: StartAction) {
    match action {
        StartAction::Search => {
            app.focused = true;
            app.address_len = 0;
            app.address[0] = 0;
            paint(app);
        }
        StartAction::Url(url) => begin_navigation(app, url, false, false),
        StartAction::Bookmark(index) => {
            let source = unsafe { &mut *core::ptr::addr_of_mut!(BASE_URL) };
            let length = unsafe {
                let bookmarks = &*core::ptr::addr_of!(BOOKMARKS);
                let length = bookmarks[index].url_len.min(URL_CAPACITY);
                source[..length].copy_from_slice(&bookmarks[index].url[..length]);
                length
            };
            let url = unsafe {
                let source = &*core::ptr::addr_of!(BASE_URL);
                core::str::from_utf8(&source[..length]).unwrap_or("")
            };
            begin_navigation(app, url, false, false);
        }
    }
}

fn show_start(app: &mut App) {
    cancel_load(app);
    clear_image(app);
    clear_hover(app);
    app.find_mode = false;
    app.page.loaded = false;
    app.page.loading = false;
    app.page.status = 0;
    app.page.error = "";
    app.scroll = 0;
    app.address_len = 0;
    app.address[0] = 0;
    app.focused = true;
    {
        let tab = &mut tabs()[app.active];
        tab.loaded = false;
        tab.status = 0;
        tab.error = "";
        tab.scroll = 0;
        tab.url_len = 0;
        tab.address_len = 0;
        tab.title_len = 0;
        tab.image_len = 0;
    }
    unsafe {
        *core::ptr::addr_of_mut!(TEXT_LEN) = 0;
        *core::ptr::addr_of_mut!(TITLE_LEN) = 0;
        *core::ptr::addr_of_mut!(LINK_RUN_COUNT) = 0;
        *core::ptr::addr_of_mut!(LINK_POOL_LEN) = 0;
    }
    paint(app);
}

fn bookmark_current(app: &mut App) {
    let tab = tab_ref(app.active);
    if !app.page.loaded || tab.url_len == 0 {
        app.toast = "OPEN A PAGE FIRST";
    } else {
        let count = unsafe { *core::ptr::addr_of!(BOOKMARK_COUNT) };
        let mut exists = false;
        unsafe {
            let bookmarks = &*core::ptr::addr_of!(BOOKMARKS);
            for index in 0..count {
                if bookmarks[index].url_len == tab.url_len
                    && bookmarks[index].url[..tab.url_len] == tab.url[..tab.url_len]
                {
                    exists = true;
                    break;
                }
            }
        }
        if exists {
            app.toast = "ALREADY SAVED";
        } else if count >= BOOKMARK_MAX {
            app.toast = "BOOKMARKS FULL";
        } else {
            let title = if tab.title_len > 0 {
                tab.title_text()
            } else {
                core::str::from_utf8(&tab.url[..tab.url_len]).unwrap_or("")
            };
            unsafe {
                let bookmarks = &mut *core::ptr::addr_of_mut!(BOOKMARKS);
                let slot = &mut bookmarks[count];
                let title_len = title.len().min(BOOKMARK_TITLE);
                slot.title[..title_len].copy_from_slice(&title.as_bytes()[..title_len]);
                slot.title_len = title_len;
                slot.url[..tab.url_len].copy_from_slice(&tab.url[..tab.url_len]);
                slot.url_len = tab.url_len;
                *core::ptr::addr_of_mut!(BOOKMARK_COUNT) = count + 1;
            }
            app.toast = "SAVED TO THE START PAGE";
        }
    }
    app.toast_until = syscall::uptime_ticks().saturating_add(TOAST_TICKS);
    paint(app);
}

fn clear_hover(app: &mut App) {
    if app.hover_len > 0 {
        app.hover_len = 0;
        window::write_u32(app.window, window::FIELD_CURSOR, 0);
    }
}

fn cancel_load(app: &mut App) {
    if app.pending.is_some() {
        app.pending = None;
        app.page.loading = false;
        app.page.error = "";
    }
}

fn switch_tab(app: &mut App, slot: usize) {
    if slot >= TAB_MAX || !tab_ref(slot).open || slot == app.active {
        return;
    }
    cancel_load(app);
    clear_hover(app);
    clear_image(app);
    app.find_mode = false;
    save_active(app);
    restore_active(app, slot);
    app.focused = !tab_ref(slot).loaded;
    paint(app);
    let image_len = tab_ref(slot).image_len;
    if image_len > 0 {
        let source = unsafe { &mut *core::ptr::addr_of_mut!(BASE_URL) };
        unsafe {
            let tab = tab_ref(slot);
            source[..image_len].copy_from_slice(&tab.image_url[..image_len]);
        }
        let text = unsafe {
            let source = &*core::ptr::addr_of!(BASE_URL);
            core::str::from_utf8(&source[..image_len]).unwrap_or("")
        };
        start_image(app, text);
    }
}

fn new_tab(app: &mut App) -> bool {
    cancel_load(app);
    save_active(app);
    let tabs = tabs();
    let Some(slot) = (0..TAB_MAX).find(|&index| !tabs[index].open) else {
        return false;
    };
    tabs[slot] = Tab::EMPTY;
    tabs[slot].open = true;
    clear_hover(app);
    clear_image(app);
    restore_active(app, slot);
    reset_active_view(app);
    paint(app);
    true
}

fn close_tab(app: &mut App, slot: usize) {
    if slot >= TAB_MAX || !tab_ref(slot).open {
        return;
    }
    let was_active = slot == app.active;
    if was_active {
        cancel_load(app);
        clear_hover(app);
        clear_image(app);
    }
    {
        let tabs = tabs();
        tabs[slot].open = false;
        tabs[slot] = Tab::EMPTY;
        if open_tab_count() == 0 {
            tabs[slot] = Tab::EMPTY;
            tabs[slot].open = true;
        }
    }
    if was_active {
        let mut next = None;
        for step in 1..=TAB_MAX {
            let candidate = (slot + step) % TAB_MAX;
            if tab_ref(candidate).open {
                next = Some(candidate);
                break;
            }
        }
        if let Some(next) = next {
            restore_active(app, next);
            app.focused = !tab_ref(next).loaded;
            let image_len = tab_ref(next).image_len;
            if image_len > 0 {
                let source = unsafe { &mut *core::ptr::addr_of_mut!(BASE_URL) };
                unsafe {
                    let tab = tab_ref(next);
                    source[..image_len].copy_from_slice(&tab.image_url[..image_len]);
                }
                let text = unsafe {
                    let source = &*core::ptr::addr_of!(BASE_URL);
                    core::str::from_utf8(&source[..image_len]).unwrap_or("")
                };
                start_image(app, text);
            }
        }
    }
    paint(app);
}

fn next_tab(app: &mut App) {
    if open_tab_count() <= 1 {
        return;
    }
    for step in 1..=TAB_MAX {
        let candidate = (app.active + step) % TAB_MAX;
        if tab_ref(candidate).open {
            switch_tab(app, candidate);
            return;
        }
    }
}

/// Resolves the address-bar text to a URL: an explicit URL is used as-is, a
/// bare host gets `https://`, anything else becomes a DuckDuckGo HTML search
/// (plain, script-free results that this renderer can read).
fn resolve_url(text: &str, buffer: &mut [u8; URL_CAPACITY]) -> Option<usize> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.starts_with("http://") || text.starts_with("https://") {
        let length = strip_fragment(text).len().min(URL_CAPACITY - 1);
        buffer[..length].copy_from_slice(&text.as_bytes()[..length]);
        return Some(length);
    }
    let looks_like_host = !text.contains(' ')
        && text.contains('.')
        && !text.contains('?')
        && !text.contains('=');
    if looks_like_host {
        let prefix = b"https://";
        let length = (prefix.len() + text.len()).min(URL_CAPACITY - 1);
        buffer[..prefix.len()].copy_from_slice(prefix);
        buffer[prefix.len()..length].copy_from_slice(&text.as_bytes()[..length - prefix.len()]);
        return Some(length);
    }

    let prefix = DEFAULT_SEARCH.as_bytes();
    let mut length = prefix.len();
    buffer[..length].copy_from_slice(prefix);
    for &byte in text.as_bytes() {
        if length + 3 >= URL_CAPACITY {
            break;
        }
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            buffer[length] = byte;
            length += 1;
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            buffer[length] = b'%';
            buffer[length + 1] = HEX[(byte >> 4) as usize];
            buffer[length + 2] = HEX[(byte & 0xF) as usize];
            length += 3;
        }
    }
    Some(length)
}

fn strip_fragment(text: &str) -> &str {
    match text.as_bytes().iter().position(|&byte| byte == b'#') {
        Some(index) => &text[..index],
        None => text,
    }
}

fn submit(app: &mut App) {
    let mut url = [0_u8; URL_CAPACITY];
    let text = app.address_text();
    let Some(length) = resolve_url(text, &mut url) else {
        return;
    };
    let target = core::str::from_utf8(&url[..length]).unwrap_or("");
    begin_navigation(app, target, false, false);
}

fn reload(app: &mut App) {
    let mut url = [0_u8; URL_CAPACITY];
    let mut length = {
        let tab = tab_ref(app.active);
        let length = tab.url_len;
        if length > 0 {
            url[..length].copy_from_slice(&tab.url[..length]);
        }
        length
    };
    if length == 0 {
        let text = app.address_text();
        let Some(resolved) = resolve_url(text, &mut url) else {
            return;
        };
        length = resolved;
    }
    let target = core::str::from_utf8(&url[..length]).unwrap_or("");
    if !target.is_empty() {
        begin_navigation(app, target, true, false);
    }
}

fn go_history(app: &mut App, delta: i32) {
    let mut url = [0_u8; URL_CAPACITY];
    let url_len = {
        let tab = &mut tabs()[app.active];
        if tab.history_len == 0 {
            return;
        }
        let target = if delta < 0 {
            tab.history_pos.checked_sub((-delta) as usize)
        } else {
            let next = tab.history_pos + delta as usize;
            if next < tab.history_len {
                Some(next)
            } else {
                None
            }
        };
        let Some(target) = target else {
            return;
        };
        tab.history_pos = target;
        let len = tab.history_lens[target] as usize;
        url[..len].copy_from_slice(&tab.history[target][..len]);
        len
    };
    let text = core::str::from_utf8(&url[..url_len]).unwrap_or("");
    set_address(app, text);
    begin_navigation(app, text, true, true);
}

fn begin_navigation(app: &mut App, url: &str, from_history: bool, keep_address: bool) {
    clear_image(app);
    app.find_mode = false;
    app.hover_len = 0;
    window::write_u32(app.window, window::FIELD_CURSOR, 0);
    app.page.loading = true;
    app.page.error = "";
    app.page.status = 0;
    app.scroll = 0;
    app.pending = None;
    if !keep_address {
        set_address(app, url);
    }
    app.focused = false;
    // Show the loading frame before a possible DHCP wait or a host round trip.
    paint(app);
    start_fetch(app, url, 0, from_history, 0);
}

fn start_fetch(
    app: &mut App,
    url: &str,
    hops: u8,
    from_history: bool,
    retries: u8,
) {
    if url.len() >= URL_CAPACITY {
        fail_page(app, "URL IS TOO LONG");
        return;
    }
    if url.starts_with("https://") {
        if window::read_u32(app.window, window::FIELD_WEB_CAPABLE) == 0 {
            fail_page(
                app,
                "HTTPS NEEDS THE HOST WINDOW (REBUILD tools/wolfser-host.sh)",
            );
            return;
        }
        let request = unsafe {
            let old = window::read_u32(app.window, window::FIELD_WEB_REQUEST_SEQ);
            let base = app.window + window::WEB_REQUEST;
            core::ptr::copy_nonoverlapping(
                url.as_ptr(),
                base as *mut u8,
                url.len(),
            );
            window::write_u32(app.window, window::FIELD_WEB_REQUEST_LEN, url.len() as u32);
            net_ipc::fence();
            let next = old.wrapping_add(1);
            window::write_u32(app.window, window::FIELD_WEB_REQUEST_SEQ, next);
            next
        };
        app.pending = Some(Pending {
            backend: Backend::Host,
            url: [0; URL_CAPACITY],
            url_len: 0,
            started: syscall::uptime_ticks(),
            hops,
            request_id: 0,
            host_seq: request,
            from_history,
            retries,
        });
        if let Some(pending) = app.pending.as_mut() {
            pending.url[..url.len()].copy_from_slice(url.as_bytes());
            pending.url_len = url.len();
        }
        return;
    }
    if !url.starts_with("http://") {
        fail_page(app, "ONLY http:// AND https:// URLS ARE SUPPORTED");
        return;
    }
    if app.server == 0 || app.net_base == 0 {
        fail_page(app, "NETWORK SERVER IS NOT RUNNING");
        return;
    }
    // A browser can be launched immediately after `howl ready`, before DHCP
    // has installed a lease. Starting the TCP fetch then makes smoltcp race
    // the interface reconfiguration (the request times out). Wait briefly for
    // the network server to report a configured address; the static QEMU
    // fallback means this never blocks forever.
    wait_for_network(app);
    if !write_request(app.net_base, url) {
        fail_page(app, "URL IS TOO LONG FOR THE NETWORK SERVER");
        return;
    }
    let request_id = unsafe {
        let next = (*core::ptr::addr_of!(REQUEST_ID)).wrapping_add(1);
        *core::ptr::addr_of_mut!(REQUEST_ID) = next;
        next
    };
    if !syscall::send(app.server, &net_ipc::http_request_id(request_id)) {
        fail_page(app, "NETWORK SERVER IS NOT RESPONDING");
        return;
    }
    app.pending = Some(Pending {
        backend: Backend::Guest,
        url: [0; URL_CAPACITY],
        url_len: 0,
        started: syscall::uptime_ticks(),
        hops,
        request_id,
        host_seq: 0,
        from_history,
        retries,
    });
    if let Some(pending) = app.pending.as_mut() {
        pending.url[..url.len()].copy_from_slice(url.as_bytes());
        pending.url_len = url.len();
    }
}

fn wait_for_network(app: &App) {
    if app.server == 0 {
        return;
    }
    let deadline = syscall::uptime_ticks().saturating_add(6 * TICKS_PER_SECOND);
    loop {
        if syscall::send(app.server, &net_ipc::ipconfig_request()) {
            if let Some(reply) =
                wait_reply(app.server, net_ipc::OP_IPCONFIG_REPLY, 50)
            {
                if let Some((configured, _, _, _, _)) = net_ipc::ipconfig_fields(&reply) {
                    if configured {
                        return;
                    }
                }
            }
        }
        if syscall::uptime_ticks() >= deadline {
            return;
        }
        syscall::sleep(100);
    }
}

fn cancel_pending(app: &mut App) {
    app.pending = None;
    app.page.loading = false;
    app.page.error = "";
    // The previous document (if any) is still in TEXT: keeping `loaded` lets
    // Escape return to it instead of blanking the window.
}

fn retryable(code: u8) -> bool {
    matches!(
        code,
        net_ipc::NET_ERR_TIMEOUT | net_ipc::NET_ERR_CONNECT | net_ipc::NET_ERR_DNS
    )
}

/// Re-sends the current navigation once or twice. The first request can race
/// DHCP/the DNS server right after boot, and a transient TLS reset looks the
/// same; a silent retry is what a browser does.
fn retry_guest(app: &mut App) -> bool {
    let Some(pending) = app.pending.as_ref() else {
        return false;
    };
    if pending.retries >= 2 {
        return false;
    }
    let retries = pending.retries;
    let hops = pending.hops;
    let from_history = pending.from_history;
    let length = pending.url_len;
    let url = unsafe { &mut *core::ptr::addr_of_mut!(BASE_URL) };
    url[..length].copy_from_slice(&pending.url[..length]);
    app.pending = None;
    syscall::sleep(40);
    let text = unsafe {
        let url = &*core::ptr::addr_of!(BASE_URL);
        core::str::from_utf8(&url[..length]).unwrap_or("")
    };
    app.page.loading = true;
    app.page.error = "";
    paint(app);
    start_fetch(app, text, hops, from_history, retries + 1);
    true
}

fn retry_host(app: &mut App) -> bool {
    let Some(pending) = app.pending.as_ref() else {
        return false;
    };
    if pending.retries >= 1 {
        return false;
    }
    let retries = pending.retries;
    let hops = pending.hops;
    let from_history = pending.from_history;
    let length = pending.url_len;
    let url = unsafe { &mut *core::ptr::addr_of_mut!(BASE_URL) };
    url[..length].copy_from_slice(&pending.url[..length]);
    app.pending = None;
    let text = unsafe {
        let url = &*core::ptr::addr_of!(BASE_URL);
        core::str::from_utf8(&url[..length]).unwrap_or("")
    };
    app.page.loading = true;
    app.page.error = "";
    start_fetch(app, text, hops, from_history, retries + 1);
    true
}

fn fail_page(app: &mut App, message: &'static str) {
    app.pending = None;
    app.page.loading = false;
    app.page.error = message;
}

fn poll_pending(app: &mut App) {
    let Some(pending) = app.pending.as_ref() else {
        return;
    };
    let backend = pending.backend;
    let started = pending.started;
    let host_seq = pending.host_seq;
    let request_id = pending.request_id;
    let now = syscall::uptime_ticks();
    if now.saturating_sub(started) > FETCH_TIMEOUT_TICKS {
        fail_page(app, "REQUEST TIMED OUT");
        return;
    }
    match backend {
        Backend::Guest => {
            let mut message = ipc::Message::new(0, &[]);
            while syscall::receive_timeout(&mut message, 0).is_ok() {
                if message.sender != app.server || message.payload[0] != net_ipc::OP_HTTP_REPLY {
                    continue;
                }
                let Some((code, status, body_len)) = net_ipc::http_fields(&message) else {
                    continue;
                };
                if net_ipc::http_id(&message) != request_id {
                    continue;
                }
                handle_guest_reply(app, code, status, body_len as usize);
                return;
            }
        }
        Backend::Host => {
            let done = window::read_u32(app.window, window::FIELD_WEB_DONE_SEQ);
            if done != host_seq {
                return;
            }
            let status = window::read_u32(app.window, window::FIELD_WEB_STATUS) as u16;
            let mut body_len = window::read_u32(app.window, window::FIELD_WEB_BODY_LEN) as usize;
            let error = window::read_u32(app.window, window::FIELD_WEB_ERROR);
            let final_len = window::read_u32(app.window, window::FIELD_WEB_FINAL_LEN) as usize;
            body_len = body_len.min(window::WEB_BODY_MAX);
            let final_len = final_len.min(window::WEB_FINAL_URL_MAX);
            let final_url = unsafe {
                let target = &mut *core::ptr::addr_of_mut!(HOST_FINAL);
                let source = (app.window + window::WEB_FINAL_URL) as *const u8;
                core::ptr::copy_nonoverlapping(
                    source,
                    target.as_mut_ptr(),
                    final_len.min(target.len()),
                );
                &*core::ptr::addr_of!(HOST_FINAL)
            };
            if error != 0 {
                if error == window::WEB_ERROR_NETWORK && retry_host(app) {
                    return;
                }
                fail_page(
                    app,
                    match error {
                        window::WEB_ERROR_NETWORK => "THE HOST COULD NOT FETCH THE PAGE",
                        window::WEB_ERROR_UNSUPPORTED => "UNSUPPORTED URL",
                        _ => "HOST FETCH FAILED",
                    },
                );
                return;
            }
            let body = unsafe {
                core::slice::from_raw_parts(
                    (app.window + window::WEB_BODY) as *const u8,
                    body_len,
                )
            };
            if is_redirect(status) {
                handle_redirect(
                    app,
                    core::str::from_utf8(&final_url[..final_len]).unwrap_or(""),
                    body,
                );
            } else {
                let url = core::str::from_utf8(&final_url[..final_len]).unwrap_or("");
                finish_page(app, url, status, body);
            }
        }
    }
}

fn handle_guest_reply(app: &mut App, code: u8, status: u16, body_len: usize) {
    let body = unsafe {
        core::slice::from_raw_parts(
            (app.net_base + net_ipc::CLIENT_BODY) as *const u8,
            body_len.min(net_ipc::CLIENT_BODY_MAX),
        )
    };
    if code == net_ipc::NET_REDIRECT {
        let base_len = {
            let base = unsafe { &mut *core::ptr::addr_of_mut!(BASE_URL) };
            let len = app
                .pending
                .as_ref()
                .map(|pending| pending.url_len)
                .unwrap_or(0);
            if len > 0 {
                if let Some(pending) = app.pending.as_ref() {
                    base[..len].copy_from_slice(&pending.url[..len]);
                }
            }
            len
        };
        let base_text = unsafe {
            let base = &*core::ptr::addr_of!(BASE_URL);
            core::str::from_utf8(&base[..base_len]).unwrap_or("")
        };
        handle_redirect(app, base_text, body);
        return;
    }
    if code != net_ipc::NET_OK {
        if retryable(code) && retry_guest(app) {
            return;
        }
        fail_page(
            app,
            match code {
                net_ipc::NET_ERR_DNS => "NAME RESOLUTION FAILED",
                net_ipc::NET_ERR_CONNECT => "CONNECTION FAILED",
                net_ipc::NET_ERR_TIMEOUT => "REQUEST TIMED OUT",
                net_ipc::NET_ERR_URL => "INVALID URL",
                net_ipc::NET_ERR_TLS => "HTTPS IS NOT AVAILABLE FOR THIS REQUEST",
                net_ipc::NET_ERR_RESPONSE => "MALFORMED HTTP RESPONSE",
                net_ipc::NET_ERR_BUSY => "NETWORK SERVER BUSY",
                _ => "NETWORK ERROR",
            },
        );
        return;
    }
    let url = app
        .pending
        .as_ref()
        .map(|pending| {
            let text = core::str::from_utf8(&pending.url[..pending.url_len]).unwrap_or("");
            let mut owned = [0_u8; URL_CAPACITY];
            owned[..text.len()].copy_from_slice(text.as_bytes());
            let len = text.len();
            (owned, len)
        })
        .unwrap_or(([0_u8; URL_CAPACITY], 0));
    let text = core::str::from_utf8(&url.0[..url.1]).unwrap_or("");
    finish_page(app, text, status, body);
}

fn handle_redirect(app: &mut App, base: &str, location: &[u8]) {
    let hops = app.pending.as_ref().map(|pending| pending.hops).unwrap_or(0);
    let from_history = app
        .pending
        .as_ref()
        .map(|pending| pending.from_history)
        .unwrap_or(false);
    if hops >= MAX_REDIRECTS {
        fail_page(app, "TOO MANY REDIRECTS");
        return;
    }
    let href = unsafe { &mut *core::ptr::addr_of_mut!(REDIRECT_HREF) };
    let count = location.len().min(href.len() - 1);
    href[..count].copy_from_slice(&location[..count]);
    let text = core::str::from_utf8(&href[..count]).unwrap_or("");
    let target = unsafe { &mut *core::ptr::addr_of_mut!(REDIRECT_TARGET) };
    let Some(len) = resolve_link(base, text, target) else {
        fail_page(app, "INVALID REDIRECT");
        return;
    };
    let url = core::str::from_utf8(&target[..len]).unwrap_or("");
    set_address(app, url);
    app.pending = None;
    start_fetch(app, url, hops + 1, from_history, 0);
}

fn finish_page(app: &mut App, url: &str, status: u16, body: &[u8]) {
    extract(body, url);
    let meta_len = unsafe { *core::ptr::addr_of!(PAGE_REDIRECT_LEN) };
    if meta_len > 0 {
        // `<meta http-equiv="refresh">` and the no-script fallback many
        // redirectors serve instead of a 3xx. Treat it like Location.
        let redirect = unsafe { &*core::ptr::addr_of!(PAGE_REDIRECT) };
        handle_redirect(app, url, &redirect[..meta_len]);
        return;
    }
    app.page.loaded = true;
    app.page.loading = false;
    app.page.status = status;
    app.page.error = "";
    app.scroll = 0;
    set_address(app, url);

    let title = unsafe { &*core::ptr::addr_of!(TITLE) };
    let title_len = unsafe { *core::ptr::addr_of!(TITLE_LEN) };
    {
        let tab = &mut tabs()[app.active];
        let length = url.len().min(URL_CAPACITY - 1);
        tab.url[..length].copy_from_slice(&url.as_bytes()[..length]);
        tab.url_len = length;
        tab.address_len = app.address_len;
        tab.address[..app.address_len].copy_from_slice(&app.address[..app.address_len]);
        tab.title_len = title_len.min(TAB_TITLE);
        tab.title[..tab.title_len].copy_from_slice(&title[..tab.title_len]);
        tab.loaded = true;
        tab.status = status;
        tab.error = "";
        let image = unsafe { &*core::ptr::addr_of!(IMAGE_URL) };
        let image_len = unsafe { *core::ptr::addr_of!(IMAGE_URL_LEN) };
        tab.image_len = image_len.min(URL_CAPACITY);
        tab.image_url[..tab.image_len].copy_from_slice(&image[..tab.image_len]);
    }

    let from_history = app
        .pending
        .as_ref()
        .map(|pending| pending.from_history)
        .unwrap_or(false);
    app.pending = None;
    if from_history {
        history_replace(app.active, url);
    } else {
        history_push(app.active, url);
    }
    start_image_from_global(app);
}

fn history_push(active: usize, url: &str) {
    if url.is_empty() {
        return;
    }
    let tab = &mut tabs()[active];
    if tab.history_len > 0 {
        let position = tab.history_pos;
        let current_len = tab.history_lens[position] as usize;
        if current_len == url.len()
            && tab.history[position][..current_len] == url.as_bytes()[..]
        {
            return;
        }
    }
    let next = if tab.history_len == 0 {
        0
    } else {
        tab.history_pos + 1
    };
    if next >= TAB_HISTORY {
        // Drop the oldest entry to make room for the new one.
        tab.history.copy_within(1.., 0);
        tab.history_lens.copy_within(1.., 0);
        let slot = TAB_HISTORY - 1;
        let len = url.len().min(URL_CAPACITY - 1);
        tab.history[slot][..len].copy_from_slice(&url.as_bytes()[..len]);
        tab.history_lens[slot] = len as u16;
        tab.history_len = TAB_HISTORY;
        tab.history_pos = slot;
        return;
    }
    let len = url.len().min(URL_CAPACITY - 1);
    tab.history[next][..len].copy_from_slice(&url.as_bytes()[..len]);
    tab.history_lens[next] = len as u16;
    tab.history_len = next + 1;
    tab.history_pos = next;
}

fn history_replace(active: usize, url: &str) {
    let tab = &mut tabs()[active];
    let position = tab.history_pos;
    let len = url.len().min(URL_CAPACITY - 1);
    tab.history[position][..len].copy_from_slice(&url.as_bytes()[..len]);
    tab.history_lens[position] = len as u16;
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

// ---------------------------------------------------------------------------
// URL resolution

fn find_byte(bytes: &[u8], needle: u8) -> Option<usize> {
    bytes.iter().position(|&byte| byte == needle)
}

fn rfind_byte(bytes: &[u8], needle: u8) -> Option<usize> {
    bytes.iter().rposition(|&byte| byte == needle)
}

/// End of `scheme://host` in a URL.
fn host_end(url: &[u8]) -> Option<usize> {
    let offset = if url.starts_with(b"https://") {
        8
    } else if url.starts_with(b"http://") {
        7
    } else {
        return None;
    };
    let mut index = offset;
    while index < url.len()
        && url[index] != b'/'
        && url[index] != b'?'
        && url[index] != b'#'
    {
        index += 1;
    }
    Some(index)
}

/// Resolves a link relative to the current page. Returns the length written
/// to `out`.
fn resolve_link(base: &str, href: &str, out: &mut [u8; URL_CAPACITY]) -> Option<usize> {
    let href = href.trim();
    let bytes = href.as_bytes();
    if bytes.is_empty()
        || href.starts_with('#')
        || href.starts_with("javascript:")
        || href.starts_with("mailto:")
    {
        return None;
    }
    let base_bytes = base.as_bytes();
    if bytes.starts_with(b"http://") || bytes.starts_with(b"https://") {
        return normalize_url(bytes, out);
    }
    if bytes.starts_with(b"//") {
        let scheme = if base_bytes.starts_with(b"https://") {
            b"https:".as_slice()
        } else {
            b"http:".as_slice()
        };
        let buffer = unsafe { &mut *core::ptr::addr_of_mut!(RESOLVE_BUFFER) };
        let mut length = scheme.len();
        buffer[..length].copy_from_slice(scheme);
        let count = bytes.len().min(buffer.len() - length);
        buffer[length..length + count].copy_from_slice(&bytes[..count]);
        length += count;
        return normalize_url(&buffer[..length], out);
    }
    let Some(host_end) = host_end(base_bytes) else {
        return None;
    };
    if bytes[0] == b'/' {
        let buffer = unsafe { &mut *core::ptr::addr_of_mut!(RESOLVE_BUFFER) };
        buffer[..host_end].copy_from_slice(&base_bytes[..host_end]);
        let count = bytes.len().min(buffer.len() - host_end);
        buffer[host_end..host_end + count].copy_from_slice(&bytes[..count]);
        return normalize_url(&buffer[..host_end + count], out);
    }

    // Strip the query and fragment from the base before joining.
    let mut base_end = base_bytes.len();
    if let Some(index) = find_byte(&base_bytes[host_end..], b'?') {
        base_end = base_end.min(host_end + index);
    }
    if let Some(index) = find_byte(&base_bytes[host_end..], b'#') {
        base_end = base_end.min(host_end + index);
    }

    if bytes[0] == b'?' {
        let buffer = unsafe { &mut *core::ptr::addr_of_mut!(RESOLVE_BUFFER) };
        buffer[..base_end].copy_from_slice(&base_bytes[..base_end]);
        let count = bytes.len().min(buffer.len() - base_end);
        buffer[base_end..base_end + count].copy_from_slice(&bytes[..count]);
        return normalize_url(&buffer[..base_end + count], out);
    }

    let directory_end = match rfind_byte(&base_bytes[host_end..base_end], b'/') {
        Some(index) => host_end + index + 1,
        None => host_end + 1,
    };
    let buffer = unsafe { &mut *core::ptr::addr_of_mut!(RESOLVE_BUFFER) };
    buffer[..directory_end].copy_from_slice(&base_bytes[..directory_end]);
    let count = bytes.len().min(buffer.len() - directory_end);
    buffer[directory_end..directory_end + count].copy_from_slice(&bytes[..count]);
    normalize_url(&buffer[..directory_end + count], out)
}

/// Copies a URL, removes dot segments and drops the fragment. Slashes and
/// the root path are preserved: `https://host/?q=x` must not become
/// `https://host?q=x` (servers treat those as different targets).
fn normalize_url(url: &[u8], out: &mut [u8; URL_CAPACITY]) -> Option<usize> {
    let end = host_end(url)?;
    let tmp = unsafe { &mut *core::ptr::addr_of_mut!(NORMALIZE_BUFFER) };
    if end >= out.len() {
        return None;
    }
    tmp[..end].copy_from_slice(&url[..end]);
    let mut length = end;
    let mut index = end;
    let path_start;
    if index < url.len() && url[index] == b'/' {
        tmp[length] = b'/';
        length += 1;
        index += 1;
        path_start = length;
    } else {
        tmp[length] = b'/';
        length += 1;
        path_start = length;
        if index < url.len() && (url[index] == b'?' || url[index] == b'#') {
            // Root path followed by a query: the slash above is the path.
        }
    }
    while index < url.len() && length + 2 < tmp.len() {
        let byte = url[index];
        if byte == b'/' {
            if length == path_start || tmp[length - 1] != b'/' {
                tmp[length] = b'/';
                length += 1;
            }
            index += 1;
            continue;
        }
        if byte == b'?' || byte == b'#' {
            let count = url.len() - index;
            if length + count < tmp.len() {
                tmp[length..length + count].copy_from_slice(&url[index..]);
                length += count;
            }
            break;
        }
        let start = index;
        while index < url.len() && url[index] != b'/' && url[index] != b'?' && url[index] != b'#' {
            index += 1;
        }
        let component = &url[start..index];
        if component == b"." {
            continue;
        }
        if component == b".." {
            if length > path_start {
                if tmp[length - 1] == b'/' {
                    length -= 1;
                }
                while length > path_start && tmp[length - 1] != b'/' {
                    length -= 1;
                }
                if length > path_start && tmp[length - 1] == b'/' {
                    length -= 1;
                }
            }
            continue;
        }
        if length + component.len() + 1 < tmp.len() {
            tmp[length..length + component.len()].copy_from_slice(component);
            length += component.len();
        }
    }
    let count = length.min(out.len() - 1);
    out[..count].copy_from_slice(&tmp[..count]);
    Some(count)
}

// ---------------------------------------------------------------------------
// HTML extraction

fn extract(body: &[u8], page_url: &str) {
    // Reader mode: when a page marks its content with <main> (or <article>),
    // show only that region. Real sites put the site header, menus and footer
    // before/after it, and the linear text renderer would otherwise start
    // every page with a screenful of navigation.
    let document = body;
    let (start, end) = content_region(body);
    let body = &body[start..end];
    let text = unsafe { &mut *core::ptr::addr_of_mut!(TEXT) };
    let styles = unsafe { &mut *core::ptr::addr_of_mut!(STYLE) };
    let runs = unsafe { &mut *core::ptr::addr_of_mut!(LINK_RUNS) };
    let pool = unsafe { &mut *core::ptr::addr_of_mut!(LINK_POOL_BYTES) };
    let title = unsafe { &mut *core::ptr::addr_of_mut!(TITLE) };

    unsafe {
        *core::ptr::addr_of_mut!(TEXT_LEN) = 0;
        *core::ptr::addr_of_mut!(TITLE_LEN) = 0;
        *core::ptr::addr_of_mut!(LINK_RUN_COUNT) = 0;
        *core::ptr::addr_of_mut!(LINK_POOL_LEN) = 0;
        *core::ptr::addr_of_mut!(PAGE_REDIRECT_LEN) = 0;
        *core::ptr::addr_of_mut!(IMAGE_URL_LEN) = 0;
    }

    let mut writer = Writer {
        text,
        styles,
        runs,
        pool,
        pool_len: 0,
        len: 0,
        line: 0,
        line_buf: [0; 96],
        line_flags: [0; 96],
        line_links: [0; 96],
        line_len: 0,
        links: [LinkRef { off: 0, len: 0 }; 32],
        link_count: 0,
        current_link: 0,
        space_pending: false,
        pending_breaks: 0,
        style_bits: 0,
        run_count: 0,
        title,
        title_len: 0,
    };

    // The <title> lives in <head>, outside the selected content region.
    if let Some(value) = find_title(document) {
        writer.push_title(value);
    }

    let mut index = 0usize;
    while index < body.len() && writer.len + 4 < TEXT_CAPACITY {
        let byte = body[index];
        if byte == b'<' {
            if body[index..].starts_with(b"<!--") {
                index = skip_comment(body, index);
                continue;
            }
            let (name, tag, after) = read_tag(body, index);
            index = after;
            let closing = tag.len() > 1 && tag[1] == b'/';
            if tag_eq(name, b"script")
                || tag_eq(name, b"style")
                || tag_eq(name, b"select")
                || tag_eq(name, b"template")
                || tag_eq(name, b"svg")
                || tag_eq(name, b"nav")
                || tag_eq(name, b"footer")
                || tag_eq(name, b"aside")
                || tag_eq(name, b"table")
                || tag_eq(name, b"sup")
            {
                if !closing {
                    let end = skip_element(body, index, name);
                    // Infoboxes and other layout tables are skipped as text,
                    // but their lead image is often the best picture on the
                    // page, so look inside before dropping the block.
                    if tag_eq(name, b"table") {
                        scan_images(body, index, end, page_url);
                    }
                    index = end;
                }
                writer.pending_breaks = writer.pending_breaks.max(1);
                continue;
            }
            if tag_eq(name, b"title") {
                if !closing {
                    let (value, after_title) = read_text_until(body, index, b"title");
                    if writer.title_len == 0 {
                        writer.push_title(value);
                    }
                    index = after_title;
                }
                continue;
            }
            if tag_eq(name, b"meta") && !closing {
                if let Some(equiv) = tag_attribute(tag, b"http-equiv") {
                    if equiv.eq_ignore_ascii_case(b"refresh") {
                        if let Some(content) = tag_attribute(tag, b"content") {
                            store_meta_redirect(content);
                        }
                    }
                }
                continue;
            }
            if tag_eq(name, b"img") && !closing {
                store_image(tag, page_url);
                continue;
            }
            if tag_eq(name, b"a") {
                if closing {
                    writer.set_link(0, 0);
                } else if let Some(href) = tag_attribute(tag, b"href") {
                    writer.store_link(href);
                }
                continue;
            }
            if is_heading(name) {
                if closing {
                    writer.style_bits &= !STYLE_HEADING;
                } else {
                    writer.pending_breaks = 2;
                    writer.style_bits |= STYLE_HEADING;
                }
                writer.pending_breaks = writer.pending_breaks.max(2);
                continue;
            }
            if tag_eq(name, b"code") || tag_eq(name, b"pre") {
                if closing {
                    writer.style_bits &= !STYLE_CODE;
                } else {
                    writer.style_bits |= STYLE_CODE;
                }
                writer.pending_breaks = writer.pending_breaks.max(1);
                continue;
            }
            if tag_eq(name, b"br") {
                writer.pending_breaks = writer.pending_breaks.max(1);
                continue;
            }
            if tag_eq(name, b"li") && !closing {
                writer.pending_breaks = writer.pending_breaks.max(1);
                writer.write_byte(b'-');
                writer.write_byte(b' ');
                continue;
            }
            if tag_eq(name, b"hr") {
                writer.pending_breaks = 2;
                continue;
            }
            if is_block_tag(name) {
                writer.pending_breaks = writer.pending_breaks.max(if is_compact_block(name) {
                    1
                } else {
                    2
                });
            }
            continue;
        }
        if byte == b'&' {
            if let Some((character, after)) = decode_entity(body, index) {
                writer.write_byte(character);
                index = after;
                continue;
            }
        }
        if byte == b'\n' || byte == b'\r' {
            writer.pending_breaks = writer.pending_breaks.max(1);
            index += 1;
            continue;
        }
        if byte == b' ' || byte == b'\t' {
            // `len` counts only flushed lines: the current line is still in
            // the wrap buffer, so test `line_len` or the first line loses all
            // of its spaces.
            if writer.line_len > 0 {
                writer.space_pending = true;
            }
            index += 1;
            continue;
        }
        if byte >= 0x80 {
            // The 5x7 font is ASCII; emit one marker per UTF-8 sequence.
            if byte >= 0xC0 {
                writer.write_byte(b'?');
            }
            index += 1;
            continue;
        }
        writer.write_byte(byte);
        index += 1;
    }

    writer.finish();
    let text_len = writer.len;
    let title_len = writer.title_len;
    let run_count = writer.run_count;
    let pool_len = writer.pool_len;
    drop(writer);
    unsafe {
        *core::ptr::addr_of_mut!(TEXT_LEN) = text_len;
        *core::ptr::addr_of_mut!(TITLE_LEN) = title_len;
        *core::ptr::addr_of_mut!(LINK_RUN_COUNT) = run_count;
        *core::ptr::addr_of_mut!(LINK_POOL_LEN) = pool_len;
    }
}

#[derive(Copy, Clone)]
struct LinkRef {
    off: u32,
    len: u16,
}

struct Writer<'a> {
    text: &'a mut [u8; TEXT_CAPACITY],
    styles: &'a mut [u8; TEXT_CAPACITY],
    runs: &'a mut [LinkRun; MAX_LINK_RUNS],
    pool: &'a mut [u8; LINK_POOL],
    pool_len: usize,
    len: usize,
    line: u16,
    line_buf: [u8; 96],
    line_flags: [u8; 96],
    line_links: [u16; 96],
    line_len: usize,
    links: [LinkRef; 32],
    link_count: usize,
    /// 0 while outside a link, otherwise `index + 1` into `links`.
    current_link: usize,
    space_pending: bool,
    pending_breaks: u8,
    style_bits: u8,
    run_count: usize,
    title: &'a mut [u8; TITLE_CAPACITY],
    title_len: usize,
}

impl<'a> Writer<'a> {
    fn flags(&self) -> u8 {
        self.style_bits | if self.current_link > 0 { STYLE_LINK } else { 0 }
    }

    fn write_byte(&mut self, byte: u8) {
        if self.pending_breaks > 0 {
            self.emit_breaks();
        }
        if self.space_pending {
            if self.line_len > 0 {
                self.write_raw(b' ', self.flags() & !STYLE_LINK);
            }
            self.space_pending = false;
        }
        self.write_raw(byte, self.flags());
    }

    fn write_raw(&mut self, byte: u8, flags: u8) {
        let limit = if flags & STYLE_HEADING != 0 {
            HEADING_COLUMNS as usize
        } else {
            TEXT_COLUMNS as usize
        };
        if self.line_len >= limit || self.line_len >= self.line_buf.len() {
            while self.line_len >= limit || self.line_len >= self.line_buf.len() {
                self.wrap_line(limit);
            }
        }
        if self.line_len >= self.line_buf.len() {
            return;
        }
        self.line_buf[self.line_len] = byte;
        self.line_flags[self.line_len] = flags;
        self.line_links[self.line_len] = self.current_link as u16;
        self.line_len += 1;
    }

    /// Emits the buffered line, breaking at the last space when possible.
    fn wrap_line(&mut self, limit: usize) {
        let mut cut = None;
        let search = self.line_len.min(limit);
        for index in (0..search).rev() {
            if self.line_buf[index] == b' ' {
                cut = Some(index);
                break;
            }
        }
        match cut {
            Some(cut) => {
                self.flush_line(cut);
                let remain = self.line_len - cut - 1;
                self.line_buf.copy_within(cut + 1..self.line_len, 0);
                self.line_flags.copy_within(cut + 1..self.line_len, 0);
                self.line_links.copy_within(cut + 1..self.line_len, 0);
                self.line_len = remain;
            }
            None => {
                self.flush_line(self.line_len);
                self.line_len = 0;
            }
        }
    }

    fn emit_breaks(&mut self) {
        let blank = self.pending_breaks >= 2;
        self.pending_breaks = 0;
        if self.line_len > 0 {
            self.flush_line(self.line_len);
            self.line_len = 0;
        }
        if blank && self.len > 0 {
            self.flush_line(0);
        }
        self.space_pending = false;
    }

    /// Appends `count` buffered characters, records link runs and ends the
    /// line with `\n`.
    fn flush_line(&mut self, count: usize) {
        let count = count.min(self.line_len);
        let mut index = 0usize;
        while index < count {
            let id = self.line_links[index];
            let start = index;
            while index < count && self.line_links[index] == id {
                index += 1;
            }
            if id != 0 && self.run_count < MAX_LINK_RUNS {
                let link = self.links[(id - 1) as usize];
                if link.len > 0 {
                    self.runs[self.run_count] = LinkRun {
                        url_off: link.off,
                        url_len: link.len,
                        line: self.line,
                        start: start as u16,
                        end: index as u16,
                    };
                    self.run_count += 1;
                }
            }
        }
        for index in 0..count {
            if self.len + 1 < TEXT_CAPACITY {
                self.text[self.len] = self.line_buf[index];
                self.styles[self.len] = self.line_flags[index];
                self.len += 1;
            }
        }
        if self.len < TEXT_CAPACITY {
            self.text[self.len] = b'\n';
            self.styles[self.len] = 0;
            self.len += 1;
        }
        self.line = self.line.wrapping_add(1);
    }

    fn finish(&mut self) {
        if self.line_len > 0 {
            self.flush_line(self.line_len);
            self.line_len = 0;
        }
        while self.len > 0 && self.text[self.len - 1] == b'\n' {
            self.len -= 1;
        }
    }

    fn set_link(&mut self, offset: u32, length: u16) {
        if length == 0 {
            self.current_link = 0;
            return;
        }
        for index in 0..self.link_count {
            if self.links[index].off == offset && self.links[index].len == length {
                self.current_link = index + 1;
                return;
            }
        }
        if self.link_count < self.links.len() {
            self.links[self.link_count] = LinkRef {
                off: offset,
                len: length,
            };
            self.link_count += 1;
            self.current_link = self.link_count;
        } else {
            self.current_link = 0;
        }
    }

    fn store_link(&mut self, href: &[u8]) {
        let mut decoded = [0_u8; 512];
        let mut length = 0usize;
        let mut index = 0usize;
        while index < href.len() && length + 1 < decoded.len() {
            let byte = href[index];
            if byte == b'&' {
                if let Some((character, after)) = decode_entity(href, index) {
                    decoded[length] = character;
                    length += 1;
                    index = after;
                    continue;
                }
            }
            decoded[length] = byte;
            length += 1;
            index += 1;
        }
        if length == 0 || self.pool_len + length > self.pool.len() {
            self.set_link(0, 0);
            return;
        }
        let offset = self.pool_len;
        self.pool[offset..offset + length].copy_from_slice(&decoded[..length]);
        self.pool_len += length;
        self.set_link(offset as u32, length as u16);
    }

    fn push_title(&mut self, value: &[u8]) {
        let mut last_space = true;
        let mut index = 0usize;
        while index < value.len() && self.title_len + 1 < self.title.len() {
            let byte = value[index];
            let character = if byte == b'&' {
                if let Some((character, after)) = decode_entity(value, index) {
                    index = after;
                    character
                } else {
                    index += 1;
                    byte
                }
            } else {
                index += 1;
                byte
            };
            if character == b'\n' || character == b'\r' || character == b'\t' || character == b' ' {
                if !last_space && self.title_len > 0 {
                    self.title[self.title_len] = b' ';
                    self.title_len += 1;
                }
                last_space = true;
            } else if (0x20..0x7F).contains(&character) {
                self.title[self.title_len] = character;
                self.title_len += 1;
                last_space = false;
            }
        }
        while self.title_len > 0 && self.title[self.title_len - 1] == b' ' {
            self.title_len -= 1;
        }
    }
}

fn content_region(body: &[u8]) -> (usize, usize) {
    if let Some((after, _)) = find_open(body, b"article") {
        return (after, skip_element_nested(body, after, b"article"));
    }
    // MediaWiki (Wikipedia and friends): the article body lives in a div
    // inside <main>, followed by the language and navigation widgets; prefer
    // that container when it is present.
    if let Some((after, name)) = find_by_id(body, b"mw-content-text") {
        return (after, skip_element_nested(body, after, name));
    }
    if let Some((after, _)) = find_open(body, b"main") {
        return (after, skip_element_nested(body, after, b"main"));
    }
    (0, body.len())
}

fn scan_images(body: &[u8], start: usize, end: usize, page_url: &str) {
    let mut index = start;
    while index < end && index < body.len() {
        if body[index] == b'<' {
            let (name, tag, after) = read_tag(body, index);
            if tag_eq(name, b"img") {
                store_image(tag, page_url);
            }
            index = after;
        } else {
            index += 1;
        }
    }
}

fn store_image(tag: &[u8], page_url: &str) {
    let already = unsafe { *core::ptr::addr_of!(IMAGE_URL_LEN) };
    if already > 0 {
        return;
    }
    let Some(src) = tag_attribute(tag, b"src")
        .or_else(|| tag_attribute(tag, b"data-src"))
    else {
        return;
    };
    if src.is_empty() {
        return;
    }
    if let Some(width) = tag_attribute(tag, b"width").and_then(attribute_number) {
        if width < 120 {
            return;
        }
    }
    if let Some(height) = tag_attribute(tag, b"height").and_then(attribute_number) {
        if height < 80 {
            return;
        }
    }
    let href = unsafe { &mut *core::ptr::addr_of_mut!(IMAGE_HREF) };
    let mut length = 0usize;
    let mut index = 0usize;
    while index < src.len() && length + 1 < href.len() {
        let byte = src[index];
        if byte == b'&' {
            if let Some((character, after)) = decode_entity(src, index) {
                href[length] = character;
                length += 1;
                index = after;
                continue;
            }
        }
        href[length] = byte;
        length += 1;
        index += 1;
    }
    let text = core::str::from_utf8(&href[..length]).unwrap_or("");
    if !looks_like_image_url(text) {
        return;
    }
    let output = unsafe { &mut *core::ptr::addr_of_mut!(IMAGE_URL) };
    if let Some(resolved) = resolve_link(page_url, text, output) {
        unsafe {
            *core::ptr::addr_of_mut!(IMAGE_URL_LEN) = resolved;
        }
    }
}

fn attribute_number(value: &[u8]) -> Option<u32> {
    let mut number = 0u32;
    let mut digits = 0usize;
    for &byte in value {
        if byte.is_ascii_digit() {
            number = number.saturating_mul(10).saturating_add(u32::from(byte - b'0'));
            digits += 1;
        } else {
            break;
        }
    }
    if digits > 0 {
        Some(number)
    } else {
        None
    }
}

fn contains_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn looks_like_image_url(url: &str) -> bool {
    let bytes = url.as_bytes();
    if url.starts_with("data:") {
        return false;
    }
    if bytes.len() > 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b".svg") {
        return false;
    }
    for bad in [
        &b"logo"[..],
        &b"icon"[..],
        &b"sprite"[..],
        &b"favicon"[..],
        &b"avatar"[..],
        &b"pixel"[..],
        &b"spacer"[..],
        &b"1x1"[..],
    ] {
        if contains_ignore_case(bytes, bad) {
            return false;
        }
    }
    true
}

fn find_title(body: &[u8]) -> Option<&[u8]> {
    let mut index = 0usize;
    while index < body.len() {
        if body[index] == b'<' {
            let (name, _, after) = read_tag(body, index);
            let closing = body.get(index + 1) == Some(&b'/');
            if !closing && tag_eq(name, b"title") {
                let (value, _) = read_text_until(body, after, b"title");
                return Some(value);
            }
            index = after;
        } else {
            index += 1;
        }
    }
    None
}

fn find_open<'a>(body: &'a [u8], name: &[u8]) -> Option<(usize, &'a [u8])> {
    let mut index = 0usize;
    while index < body.len() {
        if body[index] == b'<' {
            let (tag, _, after) = read_tag(body, index);
            let closing = body.get(index + 1) == Some(&b'/');
            if !closing && tag_eq(tag, name) {
                return Some((after, tag));
            }
            index = after;
        } else {
            index += 1;
        }
    }
    None
}

fn find_by_id<'a>(body: &'a [u8], id: &[u8]) -> Option<(usize, &'a [u8])> {
    let mut index = 0usize;
    while index < body.len() {
        if body[index] == b'<' {
            let (tag, full, after) = read_tag(body, index);
            let closing = body.get(index + 1) == Some(&b'/');
            if !closing {
                if let Some(value) = tag_attribute(full, b"id") {
                    if value == id {
                        return Some((after, tag));
                    }
                }
            }
            index = after;
        } else {
            index += 1;
        }
    }
    None
}

/// Like `skip_element`, but counts nested elements of the same name, which
/// matters for containers such as `<div id="mw-content-text">`.
fn skip_element_nested(body: &[u8], start: usize, name: &[u8]) -> usize {
    let mut depth = 1usize;
    let mut index = start;
    while index < body.len() {
        if body[index] == b'<' {
            let (tag, _, after) = read_tag(body, index);
            if tag_eq(tag, name) {
                if body.get(index + 1) == Some(&b'/') {
                    depth -= 1;
                    if depth == 0 {
                        return after;
                    }
                } else {
                    depth += 1;
                }
            }
            index = after;
        } else {
            index += 1;
        }
    }
    body.len()
}

fn is_heading(tag: &[u8]) -> bool {
    tag_eq(tag, b"h1") || tag_eq(tag, b"h2") || tag_eq(tag, b"h3")
}

fn is_compact_block(tag: &[u8]) -> bool {
    for name in [
        &b"tr"[..],
        &b"li"[..],
        &b"td"[..],
        &b"th"[..],
        &b"dd"[..],
        &b"dt"[..],
        &b"br"[..],
        &b"label"[..],
    ] {
        if tag_eq(tag, name) {
            return true;
        }
    }
    false
}

fn is_block_tag(tag: &[u8]) -> bool {
    for name in [
        &b"p"[..],
        &b"div"[..],
        &b"li"[..],
        &b"tr"[..],
        &b"td"[..],
        &b"th"[..],
        &b"dd"[..],
        &b"dt"[..],
        &b"section"[..],
        &b"article"[..],
        &b"header"[..],
        &b"footer"[..],
        &b"nav"[..],
        &b"aside"[..],
        &b"main"[..],
        &b"table"[..],
        &b"form"[..],
        &b"fieldset"[..],
        &b"ul"[..],
        &b"ol"[..],
        &b"blockquote"[..],
        &b"figure"[..],
        &b"figcaption"[..],
        &b"address"[..],
        &b"pre"[..],
        &b"h4"[..],
        &b"h5"[..],
        &b"h6"[..],
    ] {
        if tag_eq(tag, name) {
            return true;
        }
    }
    false
}

fn skip_comment(body: &[u8], start: usize) -> usize {
    let mut index = start + 4;
    while index + 2 < body.len() {
        if body[index] == b'-' && body[index + 1] == b'-' && body[index + 2] == b'>' {
            return index + 3;
        }
        index += 1;
    }
    body.len()
}

fn read_tag<'a>(body: &'a [u8], start: usize) -> (&'a [u8], &'a [u8], usize) {
    let mut index = start + 1;
    while index < body.len() && body[index] == b' ' {
        index += 1;
    }
    let name_start = if index < body.len() && body[index] == b'/' {
        index + 1
    } else {
        index
    };
    index = name_start;
    while index < body.len() && (body[index].is_ascii_alphanumeric() || body[index] == b'!') {
        index += 1;
    }
    let name_end = index;
    let mut after = index;
    let mut quote = 0u8;
    while after < body.len() {
        let byte = body[after];
        if quote != 0 {
            if byte == quote {
                quote = 0;
            }
        } else if byte == b'"' || byte == b'\'' {
            quote = byte;
        } else if byte == b'>' {
            after += 1;
            break;
        }
        after += 1;
    }
    (&body[name_start..name_end], &body[start..after], after)
}

fn tag_eq(tag: &[u8], name: &[u8]) -> bool {
    tag.len() == name.len() && tag.eq_ignore_ascii_case(name)
}

fn tag_attribute<'a>(tag: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let mut index = 1usize;
    while index < tag.len() {
        while index < tag.len()
            && (tag[index] == b' '
                || tag[index] == b'\t'
                || tag[index] == b'\n'
                || tag[index] == b'\r'
                || tag[index] == b'/')
        {
            index += 1;
        }
        if index >= tag.len() || tag[index] == b'>' {
            break;
        }
        let key_start = index;
        while index < tag.len()
            && tag[index] != b'='
            && tag[index] != b' '
            && tag[index] != b'\t'
            && tag[index] != b'\n'
            && tag[index] != b'\r'
            && tag[index] != b'>'
            && tag[index] != b'/'
        {
            index += 1;
        }
        let key = &tag[key_start..index];
        while index < tag.len() && tag[index].is_ascii_whitespace() {
            index += 1;
        }
        let mut value: &[u8] = &[];
        if index < tag.len() && tag[index] == b'=' {
            index += 1;
            while index < tag.len() && tag[index].is_ascii_whitespace() {
                index += 1;
            }
            if index < tag.len() && (tag[index] == b'"' || tag[index] == b'\'') {
                let quote = tag[index];
                index += 1;
                let value_start = index;
                while index < tag.len() && tag[index] != quote {
                    index += 1;
                }
                value = &tag[value_start..index];
                if index < tag.len() {
                    index += 1;
                }
            } else {
                let value_start = index;
                while index < tag.len()
                    && !tag[index].is_ascii_whitespace()
                    && tag[index] != b'>'
                {
                    index += 1;
                }
                value = &tag[value_start..index];
            }
        }
        if key.eq_ignore_ascii_case(name) {
            return Some(value);
        }
        if index == key_start {
            index += 1;
        }
    }
    None
}

fn skip_element(body: &[u8], start: usize, tag: &[u8]) -> usize {
    let mut index = start;
    while index < body.len() {
        if body[index] == b'<' {
            let (name, _, after) = read_tag(body, index);
            if tag_eq(name, tag) && body.get(index + 1) == Some(&b'/') {
                return after;
            }
            index = after;
        } else {
            index += 1;
        }
    }
    body.len()
}

fn read_text_until<'a>(body: &'a [u8], start: usize, tag: &[u8]) -> (&'a [u8], usize) {
    let mut index = start;
    while index < body.len() {
        if body[index] == b'<' {
            let (name, _, after) = read_tag(body, index);
            if tag_eq(name, tag) && body.get(index + 1) == Some(&b'/') {
                return (&body[start..index], after);
            }
            index = after;
        } else {
            index += 1;
        }
    }
    (&body[start..body.len().min(start + 120)], body.len())
}

fn store_meta_redirect(content: &[u8]) {
    let mut index = 0usize;
    while index + 4 <= content.len() {
        if content[index..index + 3].eq_ignore_ascii_case(b"url") && content[index + 3] == b'=' {
            index += 4;
            break;
        }
        index += 1;
    }
    if index + 4 > content.len() {
        return;
    }
    while index < content.len()
        && (content[index] == b' ' || content[index] == b'\'' || content[index] == b'"')
    {
        index += 1;
    }
    let redirect = unsafe { &mut *core::ptr::addr_of_mut!(PAGE_REDIRECT) };
    let mut length = 0usize;
    while index < content.len() && length + 1 < redirect.len() {
        let byte = content[index];
        if byte == b'"' || byte == b'\'' || byte == b';' || byte == b' ' || byte == b'\r' || byte == b'\n' {
            break;
        }
        if byte == b'&' {
            if let Some((character, after)) = decode_entity(content, index) {
                redirect[length] = character;
                length += 1;
                index = after;
                continue;
            }
        }
        redirect[length] = byte;
        length += 1;
        index += 1;
    }
    if length > 0 {
        unsafe {
            *core::ptr::addr_of_mut!(PAGE_REDIRECT_LEN) = length;
        }
    }
}

fn decode_entity(body: &[u8], start: usize) -> Option<(u8, usize)> {
    let end = body[start + 1..]
        .iter()
        .position(|&byte| byte == b';')?
        + start
        + 1;
    let name = &body[start + 1..end];
    if let Some(digits) = name.strip_prefix(b"#") {
        let (radix, digits) = if let Some(hex) = digits.strip_prefix(b"x") {
            (16, hex)
        } else {
            (10, digits)
        };
        let mut value = 0u32;
        if digits.is_empty() {
            return None;
        }
        for &digit in digits {
            let number = match digit {
                b'0'..=b'9' => u32::from(digit - b'0'),
                b'a'..=b'f' => u32::from(digit - b'a') + 10,
                b'A'..=b'F' => u32::from(digit - b'A') + 10,
                _ => return None,
            };
            value = value.saturating_mul(radix).saturating_add(number);
        }
        let character = if (0x20..0x7F).contains(&value) {
            value as u8
        } else {
            b'?'
        };
        return Some((character, end + 1));
    }
    let character = match name {
        b"amp" => b'&',
        b"lt" => b'<',
        b"gt" => b'>',
        b"quot" => b'"',
        b"apos" | b"#39" => b'\'',
        b"nbsp" | b"ensp" | b"emsp" | b"thinsp" => b' ',
        b"mdash" | b"ndash" | b"minus" => b'-',
        b"hellip" => b'.',
        b"laquo" | b"raquo" | b"lsaquo" | b"rsaquo" => b'"',
        b"lsquo" | b"rsquo" => b'\'',
        b"ldquo" | b"rdquo" => b'"',
        b"copy" => b'c',
        b"reg" => b'R',
        b"trade" => b'T',
        b"euro" => b'E',
        b"middot" | b"bull" => b'*',
        _ => return None,
    };
    Some((character, end + 1))
}

// ---------------------------------------------------------------------------
// Text layout

fn line_count() -> u32 {
    let text = unsafe { &*core::ptr::addr_of!(TEXT) };
    let len = unsafe { *core::ptr::addr_of!(TEXT_LEN) };
    let mut lines = 1u32;
    for index in 0..len {
        if text[index] == b'\n' {
            lines += 1;
        }
    }
    lines
}

fn line_height(flags: u8) -> u32 {
    if flags & STYLE_HEADING != 0 {
        HEADING_LINE_HEIGHT
    } else {
        LINE_HEIGHT
    }
}

/// Height in pixels of all lines before `line`.
fn pixels_before(line: usize) -> u32 {
    let text = unsafe { &*core::ptr::addr_of!(TEXT) };
    let styles = unsafe { &*core::ptr::addr_of!(STYLE) };
    let length = unsafe { *core::ptr::addr_of!(TEXT_LEN) };
    let mut pixels = 0u32;
    let mut index = 0usize;
    let mut current = 0usize;
    while index <= length && current < line {
        let start = index;
        while index < length && text[index] != b'\n' {
            index += 1;
        }
        let flags = if start < index { styles[start] } else { 0 };
        pixels = pixels.saturating_add(line_height(flags));
        current += 1;
        if index >= length {
            break;
        }
        index += 1;
    }
    pixels
}

// ---------------------------------------------------------------------------
// Rendering

fn paint(app: &App) {
    let base = app.window;
    fill(base, 0, 0, WIDTH, HEIGHT, COLOR_BACKGROUND);
    fill(base, 0, 0, WIDTH, HEADER_HEIGHT, COLOR_HEADER);

    // Chrome: tab strip, navigation buttons, address bar.
    draw_tabs(base, app);
    draw_button(base, BUTTON_BACK_X, "<");
    draw_button(base, BUTTON_FORWARD_X, ">");
    draw_button(base, BUTTON_RELOAD_X, "R");
    draw_button(base, BUTTON_HOME_X, "H");

    fill(base, ADDRESS_X, ADDRESS_Y, ADDRESS_WIDTH, ADDRESS_HEIGHT, COLOR_ADDRESS);
    fill(base, ADDRESS_X, ADDRESS_Y, ADDRESS_WIDTH, 1, COLOR_ADDRESS_BORDER);
    fill(base, ADDRESS_X, ADDRESS_Y + ADDRESS_HEIGHT - 1, ADDRESS_WIDTH, 1, COLOR_ADDRESS_BORDER);
    fill(base, ADDRESS_X, ADDRESS_Y, 1, ADDRESS_HEIGHT, COLOR_ADDRESS_BORDER);
    fill(
        base,
        ADDRESS_X + ADDRESS_WIDTH - 1,
        ADDRESS_Y,
        1,
        ADDRESS_HEIGHT,
        COLOR_ADDRESS_BORDER,
    );
    let address = app.address_text();
    if address.is_empty() {
        let placeholder = if app.focused {
            "TYPE A SEARCH OR AN ADDRESS, THEN PRESS ENTER"
        } else {
            "PRESS CTRL+L OR TYPE TO SEARCH"
        };
        draw_text(
            base,
            ADDRESS_X + 6,
            ADDRESS_TEXT_Y,
            1,
            placeholder,
            COLOR_PLACEHOLDER,
        );
    } else {
        let visible = (ADDRESS_WIDTH.saturating_sub(12) / 6) as usize;
        let end = core::cmp::min(address.len(), visible);
        draw_text(base, ADDRESS_X + 6, ADDRESS_TEXT_Y, 1, &address[..end], COLOR_TEXT);
    }
    if app.focused {
        let caret = ADDRESS_X + 6 + address.len() as u32 * 6;
        fill(base, caret.min(ADDRESS_X + ADDRESS_WIDTH - 3), ADDRESS_TEXT_Y - 1, 1, 10, COLOR_TEXT);
    }

    if app.page.loading {
        draw_loading(base, app);
        publish(base, window::STATE_LOADING);
        return;
    }

    if !app.page.error.is_empty() {
        draw_text(base, 24, BODY_TOP + 16, 2, "COULD NOT LOAD THE PAGE", COLOR_TEXT);
        draw_text(base, 24, BODY_TOP + 48, 1, app.page.error, COLOR_MUTED);
        let url = app.address_text();
        if !url.is_empty() {
            draw_text(base, 24, BODY_TOP + 74, 1, url, COLOR_MUTED);
        }
        draw_footer(base, app, "ERROR");
        publish(base, window::STATE_ERROR);
        return;
    }

    if !app.page.loaded {
        draw_start_page(base, app);
        draw_footer(base, app, "START");
        publish(base, window::STATE_READY);
        return;
    }

    draw_body(base, app);
    let status = if app.page.status > 0 {
        if app.page.status < 400 {
            "READY"
        } else {
            "HTTP ERROR"
        }
    } else {
        "READY"
    };
    draw_footer(base, app, status);
    publish(base, window::STATE_READY);
}

fn draw_start_page(base: u64, app: &App) {
    draw_text(base, 24, BODY_TOP + 12, 3, "WOLFSER", COLOR_TEXT);
    draw_text(
        base,
        24,
        BODY_TOP + 48,
        1,
        "SEARCH, READ AND KEEP A FEW PAGES",
        COLOR_MUTED,
    );
    for index in 0..QUICK_LINKS.len() {
        let x = TILE_X + index as u32 * (TILE_WIDTH + TILE_GAP);
        fill(base, x, TILE_Y, TILE_WIDTH, TILE_HEIGHT, COLOR_TILE);
        fill(base, x, TILE_Y, TILE_WIDTH, 1, COLOR_TILE_BORDER);
        fill(base, x, TILE_Y + TILE_HEIGHT - 1, TILE_WIDTH, 1, COLOR_TILE_BORDER);
        fill(base, x, TILE_Y, 1, TILE_HEIGHT, COLOR_TILE_BORDER);
        fill(base, x + TILE_WIDTH - 1, TILE_Y, 1, TILE_HEIGHT, COLOR_TILE_BORDER);
        draw_text(
            base,
            x + 14,
            TILE_Y + 16,
            2,
            QUICK_LINKS[index].0,
            COLOR_TEXT,
        );
        draw_text(
            base,
            x + 14,
            TILE_Y + 50,
            1,
            QUICK_HINTS[index],
            COLOR_MUTED,
        );
    }
    draw_text(base, 24, BOOKMARK_Y - 30, 2, "SAVED PAGES", COLOR_TEXT);
    let count = unsafe { *core::ptr::addr_of!(BOOKMARK_COUNT) };
    if count == 0 {
        draw_text(
            base,
            24,
            BOOKMARK_Y,
            1,
            "PRESS CTRL+D ON A PAGE TO SAVE IT HERE",
            COLOR_MUTED,
        );
    } else {
        unsafe {
            let bookmarks = &*core::ptr::addr_of!(BOOKMARKS);
            for index in 0..count {
                let bookmark = &bookmarks[index];
                let y = BOOKMARK_Y + index as u32 * BOOKMARK_ROW;
                let title = core::str::from_utf8(&bookmark.title[..bookmark.title_len])
                    .unwrap_or("");
                // The URL column starts at x=340; keep the scale-2 title clear
                // of it (24 characters is 288 px from x=24).
                let max = 24usize;
                let end = core::cmp::min(title.len(), max);
                draw_text(base, 24, y, 2, &title[..end], COLOR_TEXT);
                let url = core::str::from_utf8(&bookmark.url[..bookmark.url_len]).unwrap_or("");
                let end = core::cmp::min(url.len(), 88);
                draw_text(base, 340, y + 2, 1, &url[..end], COLOR_MUTED);
            }
        }
    }
    draw_text(
        base,
        24,
        HEIGHT - FOOTER_HEIGHT - 20,
        1,
        "CTRL+D SAVES THE CURRENT PAGE   ALT+HOME OPENS THIS START PAGE",
        COLOR_MUTED,
    );
}

fn draw_tabs(base: u64, app: &App) {
    let count = open_tab_count();
    let mut cursor = TAB_START_X;
    for screen in 0..count {
        let Some(slot) = open_slot_at(screen) else {
            break;
        };
        let tab = tab_ref(slot);
        let active = slot == app.active;
        fill(
            base,
            cursor,
            TAB_Y,
            TAB_WIDTH,
            TAB_BAR_HEIGHT,
            if active {
                COLOR_TAB_ACTIVE
            } else {
                COLOR_TAB
            },
        );
        let color = if active {
            COLOR_TAB_TEXT_ACTIVE
        } else {
            COLOR_TAB_TEXT
        };
        let title = if active && app.page.loading {
            "LOADING..."
        } else if tab.title_len > 0 {
            tab.title_text()
        } else {
            "NEW TAB"
        };
        let max = ((TAB_WIDTH - TAB_CLOSE_WIDTH - 12) / 6) as usize;
        let end = core::cmp::min(title.len(), max);
        draw_text(base, cursor + 6, TAB_TEXT_Y, 1, &title[..end], color);
        draw_text(
            base,
            cursor + TAB_WIDTH - TAB_CLOSE_WIDTH + 4,
            TAB_TEXT_Y,
            1,
            "x",
            color,
        );
        cursor += TAB_WIDTH + TAB_GAP;
    }
    // "+" button after the last tab.
    fill(
        base,
        cursor,
        TAB_Y,
        TAB_BAR_HEIGHT,
        TAB_BAR_HEIGHT,
        COLOR_TAB,
    );
    draw_text(
        base,
        cursor + 7,
        TAB_TEXT_Y,
        1,
        "+",
        COLOR_TAB_TEXT,
    );
    draw_text(
        base,
        WIDTH - 62,
        TAB_TEXT_Y,
        1,
        "WOLFSER",
        COLOR_HEADER_TITLE,
    );
}

fn draw_button(base: u64, x: u32, label: &str) {
    fill(base, x, BUTTON_Y, BUTTON_WIDTH, BUTTON_HEIGHT, COLOR_BUTTON);
    let width = (label.len() as u32 * 12).min(BUTTON_WIDTH);
    draw_text(
        base,
        x + (BUTTON_WIDTH - width) / 2,
        BUTTON_Y + 5,
        2,
        label,
        COLOR_HEADER_TEXT,
    );
}

fn draw_loading(base: u64, app: &App) {
    let spin = b"|/-\\";
    let index = (syscall::uptime_ticks() / 12) as usize % spin.len();
    draw_char(base, 24, BODY_TOP + 24, 3, spin[index] as char, COLOR_TEXT);
    draw_text(base, 52, BODY_TOP + 28, 2, "LOADING", COLOR_TEXT);
    let url = app.address_text();
    if !url.is_empty() {
        draw_text(base, 24, BODY_TOP + 64, 1, url, COLOR_MUTED);
    }
    let hint = pending_backend(app);
    draw_text(base, 24, BODY_TOP + 84, 1, hint, COLOR_MUTED);
}

fn pending_backend(app: &App) -> &'static str {
    match app.pending {
        Some(Pending {
            backend: Backend::Host,
            ..
        }) => "SECURE FETCH VIA THE HOST WINDOW",
        Some(Pending {
            backend: Backend::Guest,
            ..
        }) => "FETCH VIA THE WEREWOLF NETWORK STACK",
        None => "",
    }
}

fn draw_footer(base: u64, app: &App, status: &str) {
    fill(
        base,
        0,
        HEIGHT - FOOTER_HEIGHT,
        WIDTH,
        FOOTER_HEIGHT,
        COLOR_FOOTER,
    );
    let mut y = HEIGHT - 12;
    if app.find_mode {
        fill(
            base,
            0,
            HEIGHT - FOOTER_HEIGHT,
            WIDTH,
            FOOTER_HEIGHT,
            COLOR_FIND,
        );
        draw_text(base, 12, y, 1, "FIND:", COLOR_TEXT);
        if app.find_len > 0 {
            let query = core::str::from_utf8(&app.find_query[..app.find_len]).unwrap_or("");
            draw_text(base, 54, y, 1, query, COLOR_TEXT);
            let mut label = [0_u8; 24];
            let mut length = 0usize;
            if app.find_total > 0 {
                let (current, current_len) = digits((app.find_index + 1) as u16);
                let (total, total_len) = digits(app.find_total as u16);
                label[length] = b' ';
                length += 1;
                label[length..length + current_len].copy_from_slice(&current[..current_len]);
                length += current_len;
                label[length] = b'/';
                length += 1;
                label[length..length + total_len].copy_from_slice(&total[..total_len]);
                length += total_len;
            } else {
                label[length..length + 10].copy_from_slice(b" NO MATCH");
                length += 10;
            }
            draw_text_bytes(base, 54 + app.find_len as u32 * 6 + 8, y, 1, &label[..length], COLOR_MUTED);
        } else {
            draw_text(
                base,
                54,
                y,
                1,
                "TYPE TO SEARCH THE PAGE",
                COLOR_MUTED,
            );
        }
        draw_text(
            base,
            WIDTH - 132,
            y,
            1,
            "ENTER NEXT   ESC CLOSE",
            COLOR_MUTED,
        );
        return;
    }
    let mut label = [0_u8; 48];
    let mut length = 0usize;
    for &byte in status.as_bytes() {
        if length < label.len() {
            label[length] = byte;
            length += 1;
        }
    }
    if app.page.status > 0 {
        if length + 8 < label.len() {
            label[length..length + 6].copy_from_slice(b" HTTP ");
            length += 6;
            let (digits, count) = digits(app.page.status);
            for &byte in &digits[..count] {
                if length < label.len() {
                    label[length] = byte;
                    length += 1;
                }
            }
        }
    }
    draw_text_bytes(base, 12, y, 1, &label[..length], COLOR_MUTED);
    if !app.toast.is_empty() {
        draw_text(base, 180, y, 1, app.toast, COLOR_MUTED);
        return;
    }
    if app.hover_len > 0 {
        let max = ((WIDTH - 200) / 6) as usize;
        let end = core::cmp::min(app.hover_len, max);
        let url = core::str::from_utf8(&app.hover[..end]).unwrap_or("");
        draw_text(base, 180, y, 1, url, COLOR_LINK);
        return;
    }
    let hint = "CTRL+T NEW  CTRL+W CLOSE  CTRL+TAB NEXT  CTRL+L ADDRESS  ALT+LEFT/RIGHT  CTRL+Q QUIT";
    let width = hint.len() as u32 * 6;
    draw_text(
        base,
        WIDTH.saturating_sub(width + 12),
        y,
        1,
        hint,
        COLOR_MUTED,
    );
}

fn digits(value: u16) -> ([u8; 5], usize) {
    let mut buffer = [b'0'; 5];
    let mut index = 5usize;
    let mut value = value;
    loop {
        index -= 1;
        buffer[index] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 || index == 0 {
            break;
        }
    }
    let mut result = [0_u8; 5];
    let count = 5 - index;
    result[..count].copy_from_slice(&buffer[index..]);
    (result, count)
}

fn draw_body(base: u64, app: &App) {
    let text = unsafe { &*core::ptr::addr_of!(TEXT) };
    let styles = unsafe { &*core::ptr::addr_of!(STYLE) };
    let length = unsafe { *core::ptr::addr_of!(TEXT_LEN) };
    if length == 0 {
        draw_text(base, 24, BODY_TOP + 20, 2, "EMPTY PAGE", COLOR_TEXT);
        return;
    }
    let scroll_pixels = pixels_before(app.scroll);
    let offset = BODY_TOP as i32 - scroll_pixels as i32;
    let mut match_cursor = 0usize;
    if app.image_ready {
        let x = MARGIN as i32 + ((WIDTH - 2 * MARGIN - app.image_w) / 2) as i32;
        draw_image(base, x, offset, app.image_w, app.image_h);
    }
    let mut y = offset + image_height(app) as i32;
    let bottom = (HEIGHT - FOOTER_HEIGHT) as i32;
    let mut line = 0u32;
    let mut start = 0usize;
    while start <= length {
        let mut end = start;
        while end < length && text[end] != b'\n' {
            end += 1;
        }
        let flags = if start < end { styles[start] } else { 0 };
        let height = line_height(flags);
        // The lead image occupies space before line 0, so a line with a
        // smaller index than `scroll` can still be visible when the image is
        // scrolling away; decide by position, not by index.
        // Draw only fully visible lines. Clamping a partially scrolled line
        // to BODY_TOP would overlap the next line.
        if y >= BODY_TOP as i32 && y < bottom {
            let needle = if app.find_mode {
                &app.find_query[..app.find_len]
            } else {
                &[]
            };
            let line_matches = count_matches(&text[start..end], needle);
            let current = if app.find_mode
                && app.find_index >= match_cursor
                && app.find_index < match_cursor + line_matches
            {
                Some(app.find_index - match_cursor)
            } else {
                None
            };
            draw_styled_line(
                base,
                MARGIN,
                y as u32,
                &text[start..end],
                &styles[start..end],
                needle,
                current,
            );
            match_cursor += line_matches;
        }
        y += height as i32;
        line += 1;
        if end >= length || y >= bottom {
            break;
        }
        start = end + 1;
    }
}

fn draw_image(base: u64, x: i32, y: i32, width: u32, height: u32) {
    let top = BODY_TOP as i32;
    let bottom = (HEIGHT - FOOTER_HEIGHT) as i32;
    let mut row = 0u32;
    while row < height {
        let destination_y = y + row as i32;
        if destination_y >= top && destination_y < bottom {
            let mut column = 0u32;
            while column < width {
                let destination_x = x + column as i32;
                if destination_x >= 0 && destination_x < WIDTH as i32 {
                    let source = (base
                        + window::WEB_BODY
                        + u64::from((row * width + column) * 4))
                        as *const u32;
                    let pixel = unsafe { core::ptr::read_volatile(source) };
                    let alpha = (pixel >> 24) & 0xFF;
                    if alpha == 0xFF {
                        put_pixel(base, destination_x as u32, destination_y as u32, pixel);
                    } else if alpha != 0 {
                        // Blend transparent pixels against the white page.
                        let inverse = 255 - alpha;
                        let red = (pixel & 0xFF) * alpha + 255 * inverse;
                        let green = ((pixel >> 8) & 0xFF) * alpha + 255 * inverse;
                        let blue = ((pixel >> 16) & 0xFF) * alpha + 255 * inverse;
                        let blended = (red / 255)
                            | ((green / 255) << 8)
                            | ((blue / 255) << 16)
                            | 0xFF00_0000;
                        put_pixel(base, destination_x as u32, destination_y as u32, blended);
                    }
                }
                column += 1;
            }
        }
        row += 1;
    }
}

fn draw_styled_line(
    base: u64,
    x: u32,
    y: u32,
    bytes: &[u8],
    styles: &[u8],
    needle: &[u8],
    current: Option<usize>,
) {
    if !needle.is_empty() && needle.len() <= bytes.len() {
        let scale = match styles.first() {
            Some(flags) if flags & STYLE_HEADING != 0 => HEADING_SCALE,
            _ => SCALE,
        };
        let cell = (font5x7::GLYPH_WIDTH + 1) * scale;
        let mut index = 0usize;
        let mut occurrence = 0usize;
        while index + needle.len() <= bytes.len() {
            if bytes[index..index + needle.len()].eq_ignore_ascii_case(needle) {
                let color = if current == Some(occurrence) {
                    COLOR_MATCH_CURRENT
                } else {
                    COLOR_MATCH
                };
                fill(
                    base,
                    x + index as u32 * cell,
                    y.saturating_sub(1),
                    needle.len() as u32 * cell,
                    8 * scale,
                    color,
                );
                occurrence += 1;
                index += needle.len();
            } else {
                index += 1;
            }
        }
    }
    let mut cursor = x;
    for (index, &byte) in bytes.iter().enumerate() {
        let flags = styles.get(index).copied().unwrap_or(0);
        let scale = if flags & STYLE_HEADING != 0 {
            HEADING_SCALE
        } else {
            SCALE
        };
        let color = if flags & STYLE_LINK != 0 {
            COLOR_LINK
        } else if flags & STYLE_HEADING != 0 {
            COLOR_HEADING
        } else if flags & STYLE_CODE != 0 {
            COLOR_CODE
        } else {
            COLOR_TEXT
        };
        draw_char(base, cursor, y, scale, byte as char, color);
        if flags & STYLE_LINK != 0 {
            fill(
                base,
                cursor,
                y + 7 * scale + 1,
                font5x7::GLYPH_WIDTH * scale,
                1,
                color,
            );
        }
        cursor += (font5x7::GLYPH_WIDTH + 1) * scale;
        if cursor >= WIDTH {
            break;
        }
    }
}

fn publish(base: u64, state: u32) {
    window::write_u32(base, window::FIELD_STATE, state);
    // Publish the sequence only after every pixel write is visible to the
    // other observer (the host process); a trailing fence is too late.
    net_ipc::fence();
    let sequence = window::read_u32(base, window::FIELD_FRAME_SEQ).wrapping_add(1);
    window::write_u32(base, window::FIELD_FRAME_SEQ, sequence);
    net_ipc::fence();
}

fn fill(base: u64, x: u32, y: u32, width: u32, height: u32, color: u32) {
    let mut row = 0u32;
    while row < height && y + row < HEIGHT {
        let mut column = 0u32;
        while column < width && x + column < WIDTH {
            put_pixel(base, x + column, y + row, color);
            column += 1;
        }
        row += 1;
    }
}

fn put_pixel(base: u64, x: u32, y: u32, color: u32) {
    unsafe {
        core::ptr::write_volatile(
            (base + window::PIXELS + u64::from(y * STRIDE + x * 4)) as *mut u32,
            color,
        );
    }
}

fn draw_text(base: u64, x: u32, y: u32, scale: u32, text: &str, color: u32) {
    let mut cursor = x;
    for character in text.chars() {
        draw_char(base, cursor, y, scale, character, color);
        cursor += (font5x7::GLYPH_WIDTH + 1) * scale;
        if cursor >= WIDTH {
            break;
        }
    }
}

fn draw_text_bytes(base: u64, x: u32, y: u32, scale: u32, text: &[u8], color: u32) {
    let mut cursor = x;
    for &byte in text {
        draw_char(base, cursor, y, scale, byte as char, color);
        cursor += (font5x7::GLYPH_WIDTH + 1) * scale;
        if cursor >= WIDTH {
            break;
        }
    }
}

fn draw_char(base: u64, x: u32, y: u32, scale: u32, character: char, color: u32) {
    let glyph = font5x7::glyph(character);
    for (column, bits) in glyph.iter().enumerate() {
        for row in 0..font5x7::GLYPH_HEIGHT {
            if bits & (1 << row) != 0 {
                fill(
                    base,
                    x + column as u32 * scale,
                    y + row * scale,
                    scale,
                    scale,
                    color,
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ivshmem

fn pci_config_address(bus: u8, device: u8, function: u8, offset: u16) -> u64 {
    (u64::from(bus) << 20)
        | (u64::from(device) << 15)
        | (u64::from(function) << 12)
        | (u64::from(offset) & 0xFFC)
}

fn read_u32(address: u64) -> Option<u32> {
    syscall::pci_read32(address)
}

fn write_u32(address: u64, value: u32) -> bool {
    syscall::pci_write32(address, value)
}

fn find_ivshmem() -> Option<(u8, u8, u8)> {
    for bus in 0..=u8::MAX {
        for device in 0..32u8 {
            for function in 0..8u8 {
                let Some(id) = read_u32(pci_config_address(bus, device, function, 0x00)) else {
                    continue;
                };
                if id as u16 == IVSHMEM_VENDOR && (id >> 16) as u16 == IVSHMEM_DEVICE {
                    return Some((bus, device, function));
                }
            }
        }
    }
    None
}

fn open_window() -> Result<u64, u64> {
    let (bus, device, function) = find_ivshmem().ok_or(1u64)?;
    let command_address = pci_config_address(bus, device, function, 0x04);
    let command = read_u32(command_address).ok_or(2u64)?;
    write_u32(command_address, command | 0x2);

    let bar = pci_config_address(bus, device, function, 0x18);
    let high = pci_config_address(bus, device, function, 0x1C);
    let saved_low = read_u32(bar).ok_or(3u64)?;
    let flags = saved_low & 0xF;
    write_u32(bar, 0xFFFF_FFFF);
    write_u32(high, 0xFFFF_FFFF);
    let low_mask = read_u32(bar).ok_or(4u64)? as u64;
    let high_mask = read_u32(high).ok_or(5u64)? as u64;
    let mask = (low_mask & 0xFFFF_FFF0) | (high_mask << 32);
    let size = (!mask).wrapping_add(1);
    let used = window::PIXELS + u64::from(STRIDE) * u64::from(HEIGHT);
    if size < used || size > WINDOW_MAX_SIZE {
        return Err(6);
    }
    write_u32(bar, IVSHMEM_BAR as u32 | flags);
    write_u32(high, (IVSHMEM_BAR >> 32) as u32);

    // The proxy areas live after the pixels, so map the whole BAR. Keep the
    // mapping bounded by the device size and the rewrite limit.
    let mapping = size.min(WINDOW_MAX_SIZE);
    let base = syscall::map_phys_nc(IVSHMEM_BAR, mapping).ok_or(7u64)?;
    window::write_u32(base, window::FIELD_MAGIC, window::MAGIC);
    window::write_u32(base, window::FIELD_VERSION, window::VERSION);
    window::write_u32(base, window::FIELD_WIDTH, WIDTH);
    window::write_u32(base, window::FIELD_HEIGHT, HEIGHT);
    window::write_u32(base, window::FIELD_STRIDE, STRIDE);
    window::write_u32(base, window::FIELD_FORMAT, window::FORMAT_RGBA8888);
    window::write_u32(base, window::FIELD_FRAME_SEQ, 0);
    window::write_u32(base, window::FIELD_STATE, window::STATE_BLANK);
    window::write_u32(base, window::FIELD_INPUT_HEAD, 0);
    window::write_u32(base, window::FIELD_INPUT_TAIL, 0);
    window::write_u32(base, window::FIELD_SCROLL, 0);
    window::write_u32(base, window::FIELD_WEB_REQUEST_SEQ, 0);
    window::write_u32(base, window::FIELD_WEB_DONE_SEQ, 0);
    window::write_u32(base, window::FIELD_WEB_ERROR, window::WEB_ERROR_NONE);
    window::write_u32(base, window::FIELD_CURSOR, 0);
    // Ask the host to show the window only now, i.e. on the user's command;
    // the viewer may have been waiting invisibly since QEMU started.
    window::write_u32(base, window::FIELD_WINDOW_REQUEST, 1);
    Ok(base)
}

// ---------------------------------------------------------------------------
// Network client

fn map_net_window(server: u64) -> Option<u64> {
    if !syscall::send(server, &net_ipc::share_request()) {
        return None;
    }
    let reply = wait_reply(server, net_ipc::OP_SHARE_REPLY, 2 * TICKS_PER_SECOND)?;
    let (paddr, size) = net_ipc::share_fields(&reply)?;
    syscall::map_phys(paddr, size)
}

fn write_request(base: u64, text: &str) -> bool {
    if text.is_empty() || text.len() > net_ipc::CLIENT_REQUEST_MAX {
        return false;
    }
    unsafe {
        core::ptr::write_volatile(base as *mut u16, text.len() as u16);
        core::ptr::copy_nonoverlapping(
            text.as_ptr(),
            (base + net_ipc::CLIENT_REQUEST + 2) as *mut u8,
            text.len(),
        );
    }
    net_ipc::fence();
    true
}

fn wait_reply(server: u64, opcode: u8, timeout_ticks: u64) -> Option<ipc::Message> {
    let deadline = syscall::uptime_ticks().saturating_add(timeout_ticks);
    let mut response = ipc::Message::new(0, &[]);
    loop {
        if !syscall::task_alive(server) {
            return None;
        }
        match syscall::receive_timeout(&mut response, RESPONSE_POLL_TICKS) {
            Ok(()) if response.sender == server && response.payload[0] == opcode => {
                return Some(response)
            }
            Ok(()) => {}
            Err(_) if syscall::uptime_ticks() >= deadline => return None,
            Err(_) => {}
        }
    }
}

fn find_task_id(program: u64) -> Option<u64> {
    let mut tasks = [syscall::TaskInfo {
        id: 0,
        state: 0,
        program: 0,
        argument: 0,
    }; 32];
    let count = syscall::task_list(&mut tasks)?;
    tasks[..count.min(32)]
        .iter()
        .rev()
        .find(|task| task.program == program && task.state != 6 && task.state != 7)
        .map(|task| task.id)
}

fn fatal(status: u64, message: &str) -> ! {
    syscall::print("WOLFSER: ");
    syscall::print(message);
    syscall::print("\r\n");
    syscall::exit_with_status(status)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit_with_status(0xB0B0)
}
