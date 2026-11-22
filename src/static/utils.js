'use strict';

var PREVIEW_DEBUG = false;

var els = {};
var conn = null;
var rpc = null;

// Sidebar state: current query, loaded rows, paging base/order, selection.
var state = {
    dict: 0,
    dicts: [],
    query: '',
    items: [],
    hasMore: false,
    loading: false,
    selected: -1,
    base: 0,
    desc: false,
    gen: 0,
    total: 0,
    anchors: [],
    anchor_count: 0,
    anchor_want: 0,
    pendingMove: 0,
    k_id: null
};

// Sidebar snapshot per dictionary id, restored when switching back.
var dictStates = {};

// Visited entries, for Alt+Left / Alt+Right.
var nav = { items: [], pos: -1 };  // 'history' is a read-only window property

// Dictionary picker: selected row index within the filtered list.
var dictPopSel = 0;

var DEFAULT_WIDTH = 260;
var MIN_WIDTH = 180;
var MAX_WIDTH = 520;

function log() {
    if (PREVIEW_DEBUG && typeof (console) !== 'undefined') {
        [].unshift.call(arguments, '[preview]:');
        console.log.apply(console, arguments);
    }
}

// impl ajax over websocket
function ajax_ws(path, callbacks) {
    if (!callbacks || typeof callbacks != 'object') throw 'TypeError callbacks';
    if (conn != null) conn.close(), conn = null;

    var ws_uri = (window.location.protocol == 'https:' && 'wss://' || 'ws://') + window.location.host + path;
    var socket = new WebSocket(ws_uri);
    conn = socket;

    // Requests issued before the handshake completes are queued, not dropped.
    var pending = [];
    function flush() {
        while (pending.length && socket.readyState === WebSocket.OPEN) {
            socket.send(pending.shift());
        }
    }

    socket.onopen = function (e) {
        flush();
        if (callbacks['onopen']) callbacks['onopen'](e);
    };
    socket.onclose = callbacks['onclose'] || function () { };

    socket.onmessage = function (e) {
        log('recv:', e.data);
        var resp;
        try { resp = JSON.parse(e.data); } catch (err) { return; }
        if (resp.id) {
            var callback = callbacks[resp.id];
            if (callback) {
                delete callbacks[resp.id];
                callback(resp.result, resp.error);
            }
        } else if (resp.event) {
            var handler = callbacks[resp.event];
            if (handler) handler(resp.params, resp);
        } else {
            log('sys_err:', resp.error);
        }
    };

    // Return function used for sending AJAX requests.
    return function (callback, method) {
        if (typeof callback != 'function') throw 'TypeError callback';
        if (typeof method != 'string') throw 'TypeError method';

        var id;
        do {
            id = Math.floor(Math.random() * 100000);
        } while (callbacks[id]);

        callbacks[id] = callback;
        var params = [].slice.call(arguments, 2);
        var msg = JSON.stringify({ id: id, method: method, dict: state.dict, params: params });
        if (socket.readyState === WebSocket.OPEN) {
            socket.send(msg);
        } else if (socket.readyState === WebSocket.CONNECTING) {
            pending.push(msg);
        }
    };
}

function bind_ws() {
    rpc = ajax_ws('/api/v1/ws/', {
        onopen: function () {
            load_initials();
            // The list may have been requested before the handshake finished;
            // it is queued, but a reconnect needs the buckets refreshed.
        },
        onclose: function () {
            log('disconnected.');
            setTimeout(bind_ws, 1000);
        },
        redirect: function (params, resp) {
            log('redirect:', params[0]);
            if (resp && resp.dict !== undefined && resp.dict !== state.dict) {
                switch_dict(resp.dict, true);
            }
            navigate(params[0], true);
        }
    });
}

// ------------------------------------------------------------------ results

function clear_list() {
    state.gen++;
    state.items = [];
    state.hasMore = false;
    state.loading = false;
    state.selected = -1;
    els.list.textContent = '';
}

function highlight(word) {
    var frag = document.createDocumentFragment();
    var q = state.query;
    if (!q) {
        frag.appendChild(document.createTextNode(word));
        return frag;
    }
    var lower = word.toLowerCase();
    var needle = q.toLowerCase();
    var at = 0;
    for (; ;) {
        var hit = lower.indexOf(needle, at);
        if (hit < 0 || !needle) break;
        if (hit > at) frag.appendChild(document.createTextNode(word.slice(at, hit)));
        var mark = document.createElement('mark');
        mark.textContent = word.slice(hit, hit + needle.length);
        frag.appendChild(mark);
        at = hit + needle.length;
    }
    frag.appendChild(document.createTextNode(word.slice(at)));
    return frag;
}

function append_rows(items) {
    var frag = document.createDocumentFragment();
    items.forEach(function (item) {
        var row = document.createElement('div');
        row.className = 'word-row';
        row.dataset.id = item[1];
        row.appendChild(highlight(item[0]));
        frag.appendChild(row);
    });
    els.list.appendChild(frag);
    state.items = state.items.concat(items);
}

function load_at(offset, done) {
    if (state.loading || !rpc) { if (done) done(false); return; }
    state.loading = true;
    var gen = state.gen;
    var query = state.query;
    var desc = state.desc;
    rpc(function (result, err) {
        if (gen !== state.gen) { // stale reply after a new search/jump
            if (done) done(false);
            return;
        }
        state.loading = false;
        append_rows((result && result.items) || []);
        state.hasMore = !!(result && result.has_more);
        if (done) done(true);
        // Short page and no scrollbar: the scroll event can never fire.
        if (state.hasMore && els.list.scrollHeight <= els.list.clientHeight + 8) {
            load_more();
        }
        flush_pending();
    }, 'suggest_words', query, offset, desc);
}

function load_more(done) {
    if (!state.hasMore || state.loading) { if (done) done(false); return; }
    load_at(state.base + state.items.length, done);
}

function prepend_rows(items) {
    var frag = document.createDocumentFragment();
    items.forEach(function (item) {
        var row = document.createElement('div');
        row.className = 'word-row';
        row.dataset.id = item[1];
        row.appendChild(highlight(item[0]));
        frag.appendChild(row);
    });
    els.list.insertBefore(frag, els.list.firstChild);
    state.items = items.concat(state.items);
}

// Pull one page ending at the current base, so the list can grow upwards.
function load_prev(done) {
    if (state.loading || !rpc || state.base <= 0) { if (done) done(false, 0); return; }
    state.loading = true;
    var gen = state.gen;
    var start = state.base;
    var offset = Math.max(0, start - 20);
    var want = start - offset;
    var query = state.query;
    var desc = state.desc;
    var old_height = els.list.scrollHeight;
    rpc(function (result) {
        if (gen !== state.gen) { if (done) done(false, 0); return; }
        state.loading = false;
        var items = ((result && result.items) || []).slice(0, want);
        if (!items.length) { if (done) done(false, 0); return; }
        prepend_rows(items);
        state.base = offset;
        state.selected = state.selected < 0 ? -1 : state.selected + items.length;
        var rows = els.list.children;
        for (var i = 0; i < rows.length; i++) {
            rows[i].classList.toggle('active', i === state.selected);
        }
        els.list.scrollTop += els.list.scrollHeight - old_height;
        if (done) done(true, items.length);
        flush_pending();
    }, 'suggest_words', query, offset, desc);
}

function search(query) {
    state.query = query;
    state.base = 0;
    clear_list();
    load_at(0);
}

function jump(offset) {
    els.query.value = '';
    state.query = '';
    clear_list();
    var gen = state.gen;
    rpc(function (result) {
        if (gen !== state.gen) return;
        state.base = (result && result.offset) || 0;
        els.list.scrollTop = 0;
        load_at(state.base);
    }, 'jump', offset, state.desc);
}

// ------------------------------------------------------------------ actions

function navigate(k_id, remember) {
    if (k_id === null || k_id === undefined) return;
    if (remember !== false) record_history(k_id);
    state.k_id = parseInt(k_id, 10);
    var url = '/api/d/' + state.dict + '/item/' + k_id;
    if (els.entry.getAttribute('src') !== url) {
        els.entry.setAttribute('src', url);
    }
    mark_current(k_id);
}

function mark_current(k_id) {
    var rows = els.list.children;
    var want = String(k_id);
    for (var i = 0; i < rows.length; i++) {
        if (rows[i].dataset.id === want) {
            set_active(i);
            return;
        }
    }
}

function pick(k_id) {
    navigate(k_id, true);
    if (rpc) rpc(function () { }, 'redirect', parseInt(k_id, 10));
    els.query.focus();
}

function record_history(k_id) {
    var entry = { d: state.dict, k: parseInt(k_id, 10) };
    var top = nav.items[nav.pos];
    if (top && top.d === entry.d && top.k === entry.k) return;
    nav.items = nav.items.slice(0, nav.pos + 1);
    nav.items.push(entry);
    nav.pos = nav.items.length - 1;
}

function go_history(delta) {
    var pos = nav.pos + delta;
    if (pos < 0 || pos >= nav.items.length) return;
    nav.pos = pos;
    var entry = nav.items[pos];
    if (entry.d !== state.dict) switch_dict(entry.d, true);
    navigate(entry.k, false);
}

function random_entry() {
    if (!rpc) return;
    rpc(function (result) {
        if (!result) return;
        els.query.value = result.word;
        search(result.word);
        navigate(result.k_id, true);
    }, 'random');
}

function set_active(index) {
    var rows = els.list.children;
    if (!rows.length) {
        state.selected = -1;
        return;
    }
    if (index < 0) index = 0;
    if (index > rows.length - 1) index = rows.length - 1;
    for (var i = 0; i < rows.length; i++) {
        rows[i].classList.toggle('active', i === index);
    }
    state.selected = index;
    rows[index].scrollIntoView({ block: 'nearest' });
}

function page_step() {
    var rows = els.list.children;
    var row = rows.length ? rows[0].offsetHeight : 21;
    return Math.max(5, Math.floor(els.list.clientHeight / (row || 21)));
}

function jump_to_offset(offset, select_last) {
    clear_list();
    state.base = Math.max(0, offset);
    els.list.scrollTop = 0;
    load_at(state.base, function (ok) {
        if (!ok || !els.list.children.length) return;
        set_active(select_last ? els.list.children.length - 1 : 0);
    });
}

function flush_pending() {
    if (state.loading || !state.pendingMove) return;
    var delta = state.pendingMove;
    state.pendingMove = 0;
    move_active(delta);
}

function move_active(delta) {
    var rows = els.list.children;
    if (!rows.length) return;
    // Key repeat can outrun the network; queue the surplus instead of dropping it.
    if (state.loading) {
        state.pendingMove += delta;
        return;
    }
    var from = state.selected < 0 ? 0 : state.selected;
    var next = from + delta;
    if (next < 0) {
        // PgUp above the first loaded row: pull the page before it.
        if (state.base > 0) {
            load_prev(function (ok, added) {
                set_active(ok ? Math.max(0, from + added + delta) : 0);
            });
        } else {
            set_active(0);
        }
        return;
    }
    if (next > rows.length - 1) {
        // PgDn/End past the loaded rows: pull one more page, then keep moving.
        if (!state.hasMore) {
            set_active(rows.length - 1);
            return;
        }
        var overflow = next - (rows.length - 1);
        var at = rows.length;
        load_more(function (ok) {
            if (!ok) return;
            var grown = els.list.children.length;
            if (grown > at) set_active(Math.min(grown - 1, at - 1 + overflow));
        });
        return;
    }
    set_active(next);
    // Keep a page of history above the selection so PgUp never hits a wall.
    if (state.base > 0 && next <= 3) load_prev();
}

function pick_active() {
    var rows = els.list.children;
    if (!rows.length) return;
    var index = state.selected < 0 ? 0 : state.selected;
    pick(rows[index].dataset.id);
}

function set_order(desc) {
    state.desc = desc;
    try { localStorage.setItem('preview.desc', desc ? '1' : '0'); } catch (e) { }
    els.sortBtn.textContent = desc ? '↓' : '↑';
    els.sortBtn.title = desc ? '当前倒序，点击切正序' : '当前正序，点击切倒序';
    search(els.query.value.trim());
}

// ------------------------------------------------------------------ initials

// The server returns exactly as many anchors as asked, evenly spaced with the
// first at entry 0 and the last at the final entry. Ask for as many as fit the
// sidebar so the strip never scrolls sideways.
var ANCHOR_MIN_PX = 20;

function anchor_fit() {
    var slot = els.initials.parentElement
        ? els.initials.parentElement.clientWidth - 12
        : 0;
    var width = slot > 0 ? slot : 200;
    return Math.max(2, Math.floor((width + 2) / ANCHOR_MIN_PX));
}

function load_initials() {
    state.anchors = [];
    state.anchor_count = 0;
    render_initials();
}

function render_initials() {
    if (!rpc) return;
    var want = anchor_fit();
    if (state.anchor_count === want) {
        paint_initials();
        return;
    }
    state.anchor_want = want;
    rpc(function (rows) {
        if (state.anchor_want !== want) return;  // a newer request won
        state.anchors = rows || [];
        state.anchor_count = want;
        paint_initials();
    }, 'initials', want);
}

function paint_initials() {
    els.initials.textContent = '';
    (state.anchors || []).forEach(function (row) {
        var btn = document.createElement('button');
        btn.type = 'button';
        btn.className = 'initial';
        btn.textContent = row[0];
        btn.title = '跳到第 ' + (row[1] + 1) + ' 条（' + row[0] + '）';
        btn.dataset.offset = row[1];
        btn.addEventListener('click', function (e) {
            jump(parseInt(e.currentTarget.dataset.offset, 10));
            els.query.focus();
        });
        els.initials.appendChild(btn);
    });
}

// ------------------------------------------------------------------ layout

function set_width(px, persist) {
    var width = Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, Math.round(px)));
    document.documentElement.style.setProperty('--sidebar-width', width + 'px');
    if (persist) {
        try { localStorage.setItem('preview.sidebarWidth', String(width)); } catch (e) { }
    }
    return width;
}

function bind_splitter() {
    var dragging = false;
    els.splitter.addEventListener('pointerdown', function (e) {
        dragging = true;
        els.splitter.classList.add('dragging');
        els.splitter.setPointerCapture(e.pointerId);
        els.entry.style.pointerEvents = 'none';
        e.preventDefault();
    });
    els.splitter.addEventListener('pointermove', function (e) {
        if (!dragging) return;
        set_width(e.clientX, false);
    });
    function stop() {
        if (!dragging) return;
        dragging = false;
        els.splitter.classList.remove('dragging');
        els.entry.style.pointerEvents = '';
        var width = document.getElementById('sidebar').getBoundingClientRect().width;
        set_width(width, true);
        render_initials();
    }
    els.splitter.addEventListener('pointerup', stop);
    els.splitter.addEventListener('pointercancel', stop);
    els.splitter.addEventListener('dblclick', function () {
        set_width(DEFAULT_WIDTH, true);
    });
}

// ------------------------------------------------------------------ init

function bind_events() {
    var timer = null;
    els.query.addEventListener('input', function () {
        clearTimeout(timer);
        timer = setTimeout(function () {
            search(els.query.value.trim());
        }, 90);
    });

    els.query.addEventListener('keydown', function (e) {
        if (e.key === 'ArrowDown') {
            move_active(1);
            e.preventDefault();
        } else if (e.key === 'ArrowUp') {
            move_active(-1);
            e.preventDefault();
        } else if (e.key === 'Enter') {
            pick_active();
            e.preventDefault();
        } else if (e.key === 'Escape') {
            els.query.value = '';
            search('');
            e.preventDefault();
        } else if (e.key === 'PageDown') {
            move_active(page_step());
            e.preventDefault();
        } else if (e.key === 'PageUp') {
            move_active(-page_step());
            e.preventDefault();
        } else if ((e.key === 'Home' || e.key === 'End') && !els.query.value) {
            if (e.key === 'Home') {
                jump_to_offset(0, false);
            } else {
                jump_to_offset(Math.max(0, state.total - 20), true);
            }
            e.preventDefault();
        }
    });

    document.addEventListener('keydown', function (e) {
        if (e.key === 'F5') {
            e.preventDefault();
            random_entry();
        } else if (e.ctrlKey && !e.altKey && !e.shiftKey && (e.key === 'l' || e.key === 'L')) {
            e.preventDefault();
            els.query.focus();
            els.query.select();
        } else if (e.altKey && e.key === 'ArrowLeft') {
            e.preventDefault();
            go_history(-1);
        } else if (e.altKey && e.key === 'ArrowRight') {
            e.preventDefault();
            go_history(1);
        } else if (e.ctrlKey && !e.altKey && !e.shiftKey && (e.key === 'k' || e.key === 'K')) {
            e.preventDefault();
            if (els.dictPop.hidden) open_dict_pop();
            else close_dict_pop();
        } else if (e.altKey && !e.ctrlKey && e.key >= '1' && e.key <= '9') {
            var want = parseInt(e.key, 10) - 1;
            if (state.dicts[want]) {
                e.preventDefault();
                switch_dict(state.dicts[want].id);
            }
        }
    });

    els.list.addEventListener('click', function (e) {
        var row = e.target.closest('.word-row');
        if (!row) return;
        pick(row.dataset.id);
    });

    els.list.addEventListener('scroll', function () {
        if (els.list.scrollTop + els.list.clientHeight >= els.list.scrollHeight - 48) {
            load_more();
        } else if (els.list.scrollTop <= 4) {
            load_prev();
        }
    });

    window.addEventListener('resize', function () {
        if (state.hasMore && els.list.scrollHeight <= els.list.clientHeight + 8) {
            load_more();
        }
        render_initials();
    });

    els.sortBtn.addEventListener('click', function () {
        set_order(!state.desc);
    });

    // Keep the caret in the search box when the window regains focus.
    window.addEventListener('focus', function () {
        if (!document.activeElement || document.activeElement === document.body) {
            els.query.focus();
        }
    });
}

// ------------------------------------------------------------- dictionaries

function current_dict() {
    for (var i = 0; i < state.dicts.length; i++) {
        if (state.dicts[i].id === state.dict) return state.dicts[i];
    }
    return null;
}

function saved_dict_id() {
    var name = null;
    try { name = localStorage.getItem('preview.dict'); } catch (e) { }
    if (name) {
        for (var i = 0; i < state.dicts.length; i++) {
            if (state.dicts[i].name === name) return state.dicts[i].id;
        }
    }
    return state.dicts.length ? state.dicts[0].id : 0;
}

function update_status() {
    var d = current_dict();
    if (!d) return;
    state.total = d.count;
    var text = d.count + ' 词条 · ' + d.name;
    if (state.dicts.length > 1) {
        text += ' (' + (state.dict + 1) + '/' + state.dicts.length + ')';
    }
    els.status.textContent = text;
}

function render_dict_button() {
    var d = current_dict();
    els.dictBtn.textContent = (d ? d.name : '…') + ' \u25be';
    els.dictBtn.title = d && d.path ? d.path : '切换词典 (Ctrl+K)';
    els.dictBtn.disabled = state.dicts.length < 2;
}

function switch_dict(id, keepEntry) {
    if (!state.dicts.length || id === state.dict) return;
    if (!current_dict()) return;
    var known = false;
    for (var i = 0; i < state.dicts.length; i++) {
        if (state.dicts[i].id === id) known = true;
    }
    if (!known) return;
    dictStates[state.dict] = {
        query: state.query,
        base: state.base,
        selected: state.selected,
        k_id: state.k_id,
        scroll: els.list.scrollTop
    };
    state.dict = id;
    try { localStorage.setItem('preview.dict', current_dict().name); } catch (e) { }
    render_dict_button();
    update_status();
    load_initials();
    var s = dictStates[id] || { query: '', base: 0, selected: -1, k_id: null, scroll: 0 };
    state.query = s.query;
    els.query.value = s.query;
    clear_list();
    state.base = s.base;
    els.status.textContent = '正在读取索引…';
    load_at(s.base, function (ok) {
        update_status();
        if (!ok) return;
        if (s.selected >= 0) set_active(s.selected);
        if (s.scroll) els.list.scrollTop = s.scroll;
    });
    if (!keepEntry) {
        if (s.k_id) {
            navigate(s.k_id, false);
        } else {
            els.entry.setAttribute('src', 'about:blank');
            state.k_id = null;
        }
    }
}

function open_dict_pop() {
    if (state.dicts.length < 2) return;
    dictPopSel = 0;
    els.dictFilter.value = '';
    render_dict_rows('');
    var rows = els.dictList.children;
    for (var i = 0; i < rows.length; i++) {
        if (parseInt(rows[i].dataset.id, 10) === state.dict) dictPopSel = i;
    }
    paint_dict_sel();
    els.dictPop.hidden = false;
    els.dictPop.style.top = (els.dictRow.offsetTop + els.dictRow.offsetHeight) + 'px';
    els.dictFilter.focus();
}

function close_dict_pop() {
    els.dictPop.hidden = true;
}

function render_dict_rows(filter) {
    var f = (filter || '').toLowerCase();
    els.dictList.textContent = '';
    var shown = 0;
    state.dicts.forEach(function (d) {
        if (f && d.name.toLowerCase().indexOf(f) < 0 &&
            (d.path || '').toLowerCase().indexOf(f) < 0) {
            return;
        }
        var row = document.createElement('div');
        row.className = d.id === state.dict ? 'dict-row active' : 'dict-row';
        row.dataset.id = d.id;
        var name = document.createElement('span');
        name.className = 'name';
        name.textContent = d.name;
        var count = document.createElement('span');
        count.className = 'count';
        count.textContent = d.count;
        row.appendChild(name);
        row.appendChild(count);
        row.addEventListener('click', function () {
            close_dict_pop();
            switch_dict(d.id);
            els.query.focus();
        });
        els.dictList.appendChild(row);
        shown++;
    });
    if (dictPopSel > shown - 1) dictPopSel = Math.max(0, shown - 1);
    if (dictPopSel < 0) dictPopSel = 0;
    paint_dict_sel();
}

function paint_dict_sel() {
    var rows = els.dictList.children;
    for (var i = 0; i < rows.length; i++) {
        rows[i].classList.toggle('sel', i === dictPopSel);
    }
    if (rows[dictPopSel]) rows[dictPopSel].scrollIntoView({ block: 'nearest' });
}

function dict_pop_move(delta) {
    var n = els.dictList.children.length;
    if (!n) return;
    dictPopSel = Math.min(n - 1, Math.max(0, dictPopSel + delta));
    paint_dict_sel();
}

function dict_pop_choose() {
    var row = els.dictList.children[dictPopSel];
    if (!row) return;
    close_dict_pop();
    switch_dict(parseInt(row.dataset.id, 10));
    els.query.focus();
}

function bind_dict_picker() {
    els.dictBtn.addEventListener('click', function () {
        if (els.dictPop.hidden) open_dict_pop();
        else close_dict_pop();
    });
    els.dictFilter.addEventListener('input', function () {
        dictPopSel = 0;
        render_dict_rows(els.dictFilter.value.trim());
    });
    els.dictFilter.addEventListener('keydown', function (e) {
        if (e.key === 'ArrowDown') {
            e.preventDefault();
            dict_pop_move(1);
        } else if (e.key === 'ArrowUp') {
            e.preventDefault();
            dict_pop_move(-1);
        } else if (e.key === 'Enter') {
            e.preventDefault();
            dict_pop_choose();
        } else if (e.key === 'Escape') {
            e.preventDefault();
            close_dict_pop();
            els.query.focus();
        }
    });
}

function load_status() {
    fetch('/api/parser')
        .then(function (r) { return r.json(); })
        .then(function (data) {
            state.dicts = data.dicts || [];
            if (!state.dicts.length) return;
            var saved = saved_dict_id();
            if (saved !== state.dict) {
                switch_dict(saved);
            } else {
                update_status();
                render_dict_button();
            }
        })
        .catch(function () { });
}

window.addEventListener('DOMContentLoaded', function () {
    els.query = document.getElementById('query');
    els.list = document.getElementById('word-list');
    els.status = document.getElementById('status');
    els.initials = document.getElementById('initials');
    els.entry = document.getElementById('entry');
    els.splitter = document.getElementById('splitter');
    els.sortBtn = document.getElementById('sort-btn');
    els.dictRow = document.getElementById('dict-row');
    els.dictBtn = document.getElementById('dict-btn');
    els.dictPop = document.getElementById('dict-pop');
    els.dictFilter = document.getElementById('dict-filter');
    els.dictList = document.getElementById('dict-list');

    var savedWidth = parseInt(localStorage.getItem('preview.sidebarWidth'), 10);
    set_width(savedWidth > 0 ? savedWidth : DEFAULT_WIDTH, false);

    state.desc = localStorage.getItem('preview.desc') === '1';
    els.sortBtn.textContent = state.desc ? '↓' : '↑';
    els.sortBtn.title = state.desc ? '当前倒序，点击切正序' : '当前正序，点击切倒序';

    bind_events();
    bind_splitter();
    bind_dict_picker();
    bind_ws();
    load_status();
    // GoldenDict-style: the whole headword list is browsable before typing.
    search('');
    els.query.focus();
    setTimeout(function () { els.query.focus(); }, 0);
});
