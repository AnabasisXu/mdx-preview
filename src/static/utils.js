'use strict';

var PREVIEW_DEBUG = true;
var JS_RELOAD = true;
var forEach = Array.prototype.forEach;

var blah = function () { return; };
var conn = null;
var ajax = null;

// make sure debug enabled!
var log = function () {
    if (PREVIEW_DEBUG && typeof (console) !== 'undefined') {
        [].unshift.call(arguments, '[preview]:');
        console.log.apply(console, arguments);
    }
}

// test for keyboard shortcut keys
var is_mac = function () {
    return navigator.userAgent.indexOf('Mac OS X') != -1;
}

// live reload
var live_reload = function () {
    function reload_css(el, file_name) {
        var url = el.getAttribute('href').split('?')[0];
        if (!url || !(url.toUpperCase().indexOf(file_name.toUpperCase()) > -1)) return;

        var new_el = document.createElement('link');
        new_el['href'] = url + '?' + Date.now();
        new_el['rel'] = 'stylesheet';

        new_el.onload = function () {
            if (new_el.parentNode && new_el.parentNode.contains(el)) new_el.parentNode.removeChild(el);
        };
        el.parentNode.insertBefore(new_el, el.nextSibling);
        return true;
    }

    return function (file_name) {
        // reloads the current page from the server
        if (JS_RELOAD && file_name.indexOf(".js") > -1) {
            document.getElementById('k_iframe').contentWindow.location.reload(true);
        }
        // reload_css
        if (file_name.indexOf(".css") > -1) {
            var elements = document.getElementById('k_iframe').contentWindow.document.querySelectorAll('link');
            forEach.call(elements, function (el) {
                reload_css(el, file_name) && log("livereload:", file_name);
            });
        }
    };
}();

// impl ajax over websocket
var ajax_ws = function (path, callbacks) {
    if (!callbacks || typeof callbacks != 'object') throw "TypeError callbacks";
    if (conn != null) conn.close(), conn = null;

    var ws_uri = (window.location.protocol == 'https:' && 'wss://' || 'ws://') + window.location.host + path;
    conn = new WebSocket(ws_uri);

    // handle websocket events
    callbacks['onopen'] ? conn.onopen = callbacks['onopen'] : conn.onopen = blah;
    callbacks['onclose'] ? conn.onclose = callbacks['onclose'] : conn.onclose = blah;

    conn.onmessage = function (e) {
        log('recv:', e.data);
        var resp = JSON.parse(e.data);
        if (resp.id) {
            var callback = callbacks[resp.id]; // callback match
            var elapsed_time = new Date() - callbacks[resp.id].start_time;
            log('elapsed_time: ' + elapsed_time + 'ms');
            delete callbacks[resp.id]; // release
            callback && callback(resp.result, resp.error);
        } else if (resp.event) { // notification from server
            var callback = callbacks[resp.event];
            callback && callback(resp.params);
        } else {
            log('sys_err:', resp.error);
        }
    };

    // Return function used for sending AJAX requests.
    return function (callback, method) {
        if (typeof callback != "function") throw "TypeError callback";
        if (typeof method != "string") throw "TypeError method";

        var id; // Find a unique ID
        do {
            id = Math.floor(Math.random() * 100000);
        } while (callbacks[id]); // while collisions

        callbacks[id] = callback; // Save callback, where rpc id is the key.
        callbacks[id].start_time = new Date();
        var params = [].slice.call(arguments, 2);
        var req = { id: id, method: method, params: params };
        conn.send(JSON.stringify(req));
    }
}

// lookup suggest words
var lookup_suggest_words = function (query, done) {
    if (ajax == null) return;
    var suggest_words_callback = function (result, err) {
        var ret = { suggestions: [] };
        result instanceof Array && result.forEach(function (x) {
            ret.suggestions.push({
                value: x[0],
                k_id: x[1]
            });
        })
        done(ret);
    }
    ajax(suggest_words_callback, "suggest_words", query.trim());
}

// force all clients redirect to the same page
var redirect = function (k_id) {
    var iframe = document.getElementById('k_iframe');
    var item_url = "/api/item/" + k_id;
    if (iframe.src != item_url) {
        iframe.src = item_url;
    }
}

var show_suggests = function () {
    let autocomplete = document.getElementById("autocomplete");
    let cover = document.getElementById("cover");
    cover.style.display = "block";
    autocomplete.focus();
    typeof (Storage) !== "undefined" && localStorage.setItem("open", 1);
}

var hide_suggests = function () {
    // Search bar stays resident; ESCAPE only clears the query and dropdown.
    let autocomplete = document.getElementById("autocomplete");
    autocomplete.value = "";
    autocomplete.blur();
}

// documenty ready
$(function () {
    log('JS_RELOAD:', JS_RELOAD);
    if (conn == null) {
        ajax = ajax_ws(
            '/api/v1/ws/',
            {
                'filechanged': function (params) {
                    log('filechanged:', params[0]);
                    live_reload(params[0]);
                },
                'redirect': function (params) {
                    log('redirect:', params[0]);
                    redirect(params[0]);
                },
                'onopen': function () {
                    log('connected.');
                    $('#status').hide();
                },
                'onclose': function () {
                    log('disconnected.');
                }
            }
        );
    }

    // Initialize autocomplete with local lookup:
    $('#autocomplete').devbridgeAutocomplete({
        lookup: lookup_suggest_words,
        minChars: 1,
        maxHeight: 230,
        appendTo: "#wrap",
        autoSelectFirst: true,
        onSelect: function (suggestion) {
            log("selected: ", suggestion.k_id, " -> ", suggestion.value);
            ajax(blah, "redirect", suggestion.k_id);
        },
        showNoSuggestionNotice: true,
        noSuggestionNotice: 'No matching results',
    });

    // Search bar is resident: show it on every page load.
    document.getElementById('k_iframe').onload = function () {
        show_suggests();
        document.getElementById('k_iframe').contentWindow.document.addEventListener('keydown', function (evt) {
            if (evt.altKey && (is_mac() && evt.key == '©' || !is_mac() && evt.key == 'g')) {
                show_suggests();
                evt.preventDefault();
            }
            if (evt.key == 'Escape') {
                hide_suggests();
            }
        }, false);
    };

    document.addEventListener('keydown', function (evt) {
        if (evt.key == 'Escape') {
            hide_suggests();
        }
        if (evt.altKey && (is_mac() && evt.key == '©' || !is_mac() && evt.key == 'g')) {
            show_suggests();
            evt.preventDefault();
        }
    });
});