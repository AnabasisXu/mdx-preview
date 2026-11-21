'use strict';

var PREVIEW_DEBUG = false;

var els = {};
var conn = null;
var rpc = null;

// UI state: query text, rendered rows, paging and the selected row.
var state = {
    query: '',
    items: [],
    hasMore: false,
    loading: false,
    selected: -1,
    zoom: 1
};

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
            if (handler) handler(resp.params);
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
        var msg = JSON.stringify({ id: id, method: method, params: params });
        if (socket.readyState === WebSocket.OPEN) {
            socket.send(msg);
        } else if (socket.readyState === WebSocket.CONNECTING) {
            pending.push(msg);
        }
    };
}

function bind_ws() {
    var callbacks = {
        onclose: function () {
            log('disconnected.');
            setTimeout(bind_ws, 1000);
        },
        redirect: function (params) {
            log('redirect:', params[0]);
            navigate(params[0]);
        }
    };
    rpc = ajax_ws('/api/v1/ws/', callbacks);
}

// ------------------------------------------------------------------ results

function clear_list() {
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

function request_page(offset) {
    if (state.loading || !rpc) return;
    state.loading = true;
    var query = state.query;
    rpc(function (result, err) {
        state.loading = false;
        if (query !== state.query) return; // stale reply
        var items = (result && result.items) || [];
        append_rows(items);
        state.hasMore = !!(result && result.has_more);
        // Short page and no scrollbar: the scroll event can never fire.
        if (state.hasMore && els.list.scrollHeight <= els.list.clientHeight + 8) {
            load_more();
        }
    }, 'suggest_words', query, offset);
}

function search(query) {
    state.query = query;
    clear_list();
    request_page(0);
}

function load_more() {
    if (!state.hasMore || state.loading) return;
    request_page(state.items.length);
}

// ------------------------------------------------------------------ actions

function navigate(k_id) {
    if (k_id === null || k_id === undefined) return;
    var url = '/api/item/' + k_id;
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
    navigate(k_id);
    if (rpc) rpc(function () { }, 'redirect', parseInt(k_id, 10));
    els.query.focus();
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

function move_active(delta) {
    if (!els.list.children.length) return;
    var next = state.selected < 0 ? 0 : state.selected + delta;
    if (next < 0) next = 0;
    set_active(next);
}

function pick_active() {
    var rows = els.list.children;
    if (!rows.length) return;
    var index = state.selected < 0 ? 0 : state.selected;
    pick(rows[index].dataset.id);
}

// ------------------------------------------------------------------ zoom

function apply_zoom() {
    if (els.entry.contentWindow) {
        els.entry.contentWindow.postMessage({ type: 'zoom', value: state.zoom }, '*');
    }
}

function set_zoom(value) {
    state.zoom = Math.round(Math.min(2, Math.max(0.8, value)) * 100) / 100;
    try { localStorage.setItem('preview.zoom', String(state.zoom)); } catch (e) { }
    apply_zoom();
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
        }
    });

    els.entry.addEventListener('load', apply_zoom);
    window.addEventListener('resize', function () {
        if (state.hasMore && els.list.scrollHeight <= els.list.clientHeight + 8) {
            load_more();
        }
    });
    window.addEventListener('message', function (e) {
        var d = e.data;
        if (d && d.type === 'ready') apply_zoom();
    });

    document.getElementById('zoom-in').addEventListener('click', function () {
        set_zoom(state.zoom + 0.1);
    });
    document.getElementById('zoom-out').addEventListener('click', function () {
        set_zoom(state.zoom - 0.1);
    });

    document.addEventListener('wheel', function (e) {
        if (!e.ctrlKey) return;
        e.preventDefault();
        set_zoom(state.zoom + (e.deltaY < 0 ? 0.1 : -0.1));
    }, { passive: false });
}

function load_status() {
    fetch('/api/parser')
        .then(function (r) { return r.json(); })
        .then(function (data) {
            els.status.textContent = data.count + ' 词条 · ' + data.encoding;
        })
        .catch(function () { });
}

window.addEventListener('DOMContentLoaded', function () {
    els.query = document.getElementById('query');
    els.list = document.getElementById('word-list');
    els.status = document.getElementById('status');
    els.entry = document.getElementById('entry');
    els.splitter = document.getElementById('splitter');

    var savedWidth = parseInt(localStorage.getItem('preview.sidebarWidth'), 10);
    set_width(savedWidth > 0 ? savedWidth : DEFAULT_WIDTH, false);

    var savedZoom = parseFloat(localStorage.getItem('preview.zoom'));
    state.zoom = Math.min(2, Math.max(0.8, savedZoom > 0 ? savedZoom : 1));

    bind_events();
    bind_splitter();
    bind_ws();
    load_status();
    // GoldenDict-style: the whole headword list is browsable before typing.
    search('');
    els.query.focus();
});
