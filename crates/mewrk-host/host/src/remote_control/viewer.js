(function () {
  'use strict';

  var T = JSON.parse(document.getElementById('i18n').textContent);
  var $ = function (id) { return document.getElementById(id); };
  var stage = $('stage');
  var picture = $('screen');
  var keys = $('keys');

  var POLL_MS = 1000;
  var MOVE_MS = 33;
  var BATCH_MAX = 200;
  var QUEUE_MAX = 500;
  var PASTE_MAX = 20000;
  var STREAM_RETRY_MS = 1500;
  var ANSWER_WAIT_MS = 2500;
  var POLL_FAILURES_UNTIL_CLOSED = 3;

  var PHASES = {
    connecting: { text: T.phase_connecting, tone: 'info' },
    credentials: { text: T.phase_credentials, tone: 'warn' },
    ready: { text: T.phase_ready, tone: 'ok' },
    reconnecting: { text: T.phase_reconnecting, tone: 'warn' },
    failed: { text: T.phase_failed, tone: 'bad' }
  };

  // Key names of the host's vocabulary; "ctrl+alt+Delete" is a chord.
  var TOOLS = {
    android: [['Back', T.tool_back], ['HomeScreen', T.tool_home], ['AppSwitch', T.tool_recents]],
    ios: [['HomeScreen', T.tool_home]],
    desktop: [['ctrl+alt+Delete', T.tool_cad]]
  };

  var viewer = newId();
  var state = null;
  // The server is gone or says the session is over.
  var closed = false;
  var failures = 0;
  var polling = false;

  function newId() {
    var bytes = new Uint8Array(12);
    if (window.crypto && window.crypto.getRandomValues) {
      window.crypto.getRandomValues(bytes);
    } else {
      for (var i = 0; i < bytes.length; i++) bytes[i] = Math.floor(Math.random() * 256);
    }
    return Array.prototype.map.call(bytes, function (b) {
      return (b < 16 ? '0' : '') + b.toString(16);
    }).join('');
  }

  function post(endpoint, body, options) {
    return fetch(endpoint, Object.assign({
      method: 'POST',
      headers: { 'Content-Type': 'text/plain' },
      body: JSON.stringify(body),
      cache: 'no-store',
      credentials: 'omit'
    }, options));
  }

  function canInput() {
    return !!state && !closed && state.phase === 'ready' && !state.readOnly && !state.modelActive;
  }

  function overlayShown() {
    return !$('overlay').hidden;
  }

  // ---------------------------------------------------------------- state

  function poll() {
    if (polling || document.hidden) return;
    polling = true;
    fetch('state', { cache: 'no-store', credentials: 'omit' })
      .then(function (response) {
        if (response.status === 410) {
          closed = true;
          stopStream();
          return null;
        }
        if (!response.ok) throw new Error('state');
        return response.json();
      })
      .then(function (next) {
        if (next) {
          failures = 0;
          state = next;
          if (closed) {
            closed = false;
            startStream();
          }
        }
        render();
      })
      .catch(function () {
        failures += 1;
        if (failures >= POLL_FAILURES_UNTIL_CLOSED && !closed) {
          closed = true;
          render();
        }
      })
      .then(function () { polling = false; });
  }

  function render() {
    if (state) {
      document.title = state.name;
      $('name').textContent = state.name;
      $('name').title = state.label ? state.name + ' · ' + state.label : state.name;
      $('size').textContent = state.screen ? state.screen.width + '×' + state.screen.height : '';
      $('readonly').hidden = !state.readOnly;
      $('model').hidden = !state.modelActive;
    }
    var phase = closed ? PHASES.failed : (state && PHASES[state.phase]);
    var badge = $('phase');
    badge.textContent = phase ? phase.text : '';
    badge.dataset.tone = phase ? phase.tone : 'muted';
    renderTools();
    renderOverlay();
    updateWaiting();
    if (!closed && !document.hidden && !picture.getAttribute('src')) startStream();
  }

  var toolsFor = null;

  function renderTools() {
    var platform = state ? state.platform : null;
    var nav = $('tools');
    if (platform !== toolsFor) {
      toolsFor = platform;
      nav.textContent = '';
      (TOOLS[platform] || []).forEach(function (tool) {
        var button = document.createElement('button');
        button.type = 'button';
        button.dataset.name = tool[0];
        button.textContent = tool[1];
        nav.appendChild(button);
      });
    }
    var enabled = canInput();
    Array.prototype.forEach.call(nav.children, function (button) { button.disabled = !enabled; });
  }

  // A press on a toolbar button must not take the keyboard away from the picture.
  $('tools').addEventListener('mousedown', function (event) { event.preventDefault(); });
  $('tools').addEventListener('click', function (event) {
    var button = event.target.closest ? event.target.closest('button') : null;
    if (!button || button.disabled || !canInput()) return;
    push({ t: 'button', name: button.dataset.name });
    focusKeys();
  });

  function updateWaiting() {
    var waiting = !!state && !closed && state.phase === 'ready' && !(picture.naturalWidth > 0);
    $('waiting').hidden = !waiting;
  }

  // -------------------------------------------------------------- overlay

  var formKey = '';
  var submitting = false;
  var submittedAt = 0;

  function renderOverlay() {
    var mode = 'none';
    if (closed) mode = 'closed';
    else if (state && state.phase !== 'ready') mode = state.phase;
    if (mode === 'credentials' && !state.credentials) mode = 'connecting';

    stage.classList.toggle('covered', mode !== 'none');
    $('overlay').hidden = mode === 'none';
    $('signin').hidden = mode !== 'credentials';
    $('busy').hidden = mode === 'none' || mode === 'credentials';

    if (mode === 'credentials') {
      showForm();
      return;
    }
    // Leaving the sign-in: the next one starts clean.
    formKey = '';
    if (submitting) setSubmitting(false);
    $('password').value = '';
    if (mode === 'none') return;

    var failed = mode === 'failed' || mode === 'closed';
    var title = { connecting: T.connecting, reconnecting: T.reconnecting, failed: T.failed, closed: T.closed }[mode];
    var detail = '';
    if (mode === 'failed') detail = (state && state.message) || T.no_details;
    else if (state && state.message && mode !== 'closed') detail = state.message;
    $('busy-title').textContent = title;
    $('busy-detail').textContent = detail;
    $('busy-detail').hidden = !detail;
    $('spinner').hidden = failed;
    $('busy').dataset.tone = failed ? 'bad' : 'info';
  }

  function showForm() {
    var request = state.credentials;
    var key = JSON.stringify([state.name, state.endpoint, request.needsUsername, request.username, request.retry]);
    if (key !== formKey) {
      formKey = key;
      $('where').textContent = state.name + ' · ' + state.endpoint;
      $('username-row').hidden = !request.needsUsername;
      $('username').value = request.username || '';
      $('password').value = '';
      $('refused').hidden = !request.retry;
      $('refused').textContent = request.retry ? T.signin_refused : '';
      setSubmitting(false);
      focusField();
    } else if (submitting && Date.now() - submittedAt > ANSWER_WAIT_MS) {
      // Still asking after a fair wait: the answer did not take.
      setSubmitting(false);
      $('password').value = '';
      focusField();
    }
  }

  function focusField() {
    var wantsName = !$('username-row').hidden && !$('username').value;
    (wantsName ? $('username') : $('password')).focus();
  }

  function setSubmitting(on) {
    submitting = on;
    ['username', 'password', 'remember', 'submit'].forEach(function (id) { $(id).disabled = on; });
    $('submit').textContent = on ? T.signin_busy : T.signin_submit;
  }

  // The password goes as base64 of its UTF-8: never a JSON string with escapes,
  // which the host would have to unescape into memory it cannot wipe.
  function utf8Base64(text) {
    var bytes = new TextEncoder().encode(text);
    var binary = '';
    for (var i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
    return btoa(binary);
  }

  $('signin').addEventListener('submit', function (event) {
    event.preventDefault();
    if (submitting || !state || !state.credentials) return;
    var body = { passwordBase64: utf8Base64($('password').value), remember: $('remember').checked };
    if (!$('username-row').hidden) body.username = $('username').value;
    setSubmitting(true);
    submittedAt = Date.now();
    post('credentials', body)
      .then(function (response) {
        if (!response.ok) setSubmitting(false);
      })
      .catch(function () { setSubmitting(false); })
      .then(function () {
        // The password does not stay in the page once it has been sent.
        $('password').value = '';
        body = null;
      });
  });

  // ---------------------------------------------------------------- stream

  function startStream() {
    if (document.hidden || closed) return;
    picture.src = 'stream?viewer=' + viewer + '&t=' + Date.now();
  }

  function stopStream() {
    picture.removeAttribute('src');
  }

  picture.addEventListener('load', updateWaiting);
  picture.addEventListener('error', function () {
    if (!picture.getAttribute('src')) return;
    stopStream();
    setTimeout(startStream, STREAM_RETRY_MS);
  });

  document.addEventListener('visibilitychange', function () {
    if (document.hidden) {
      releaseAll();
      stopStream();
      post('visibility', { viewer: viewer, visible: false }, { keepalive: true }).catch(function () {});
    } else {
      post('visibility', { viewer: viewer, visible: true }).catch(function () {});
      startStream();
      poll();
    }
  });

  // ----------------------------------------------------------------- input

  var seq = 0;
  var inflight = false;
  var queue = [];

  function clampNotches(value) {
    return Math.max(-10, Math.min(10, value));
  }

  function push(event) {
    var last = queue[queue.length - 1];
    if (last && event.t === 'pointer' && event.phase === 'move' && last.t === 'pointer' &&
        last.phase === 'move' && last.buttons === event.buttons) {
      queue[queue.length - 1] = event;
    } else if (last && event.t === 'wheel' && last.t === 'wheel') {
      last.x = event.x;
      last.y = event.y;
      last.dx = clampNotches(last.dx + event.dx);
      last.dy = clampNotches(last.dy + event.dy);
    } else if (queue.length < QUEUE_MAX || event.t !== 'pointer' || event.phase !== 'move') {
      queue.push(event);
    }
    pump();
  }

  // One batch in flight; what happens meanwhile goes out together next.
  function pump() {
    if (inflight || queue.length === 0) return;
    var events = queue.splice(0, BATCH_MAX);
    inflight = true;
    post('input', { viewer: viewer, seq: ++seq, events: events })
      .catch(function () {})
      .then(function () {
        inflight = false;
        pump();
      });
  }

  // Where the picture really is inside the element: object-fit: contain leaves
  // bars at the sides or the top.
  function pictureRect() {
    var box = picture.getBoundingClientRect();
    var width = picture.naturalWidth;
    var height = picture.naturalHeight;
    if ((!width || !height) && state && state.screen) {
      width = state.screen.width;
      height = state.screen.height;
    }
    if (!width || !height || !box.width || !box.height) return null;
    var scale = Math.min(box.width / width, box.height / height);
    var w = width * scale;
    var h = height * scale;
    return { left: box.left + (box.width - w) / 2, top: box.top + (box.height - h) / 2, width: w, height: h };
  }

  // The event's place as fractions of the picture, or null outside it (unless
  // `clamp`, for a drag that left it).
  function locate(event, clamp) {
    var rect = pictureRect();
    if (!rect) return null;
    var x = (event.clientX - rect.left) / rect.width;
    var y = (event.clientY - rect.top) / rect.height;
    if (x < 0 || x > 1 || y < 0 || y > 1) {
      if (!clamp) return null;
      x = Math.max(0, Math.min(1, x));
      y = Math.max(0, Math.min(1, y));
    }
    return { x: x, y: y };
  }

  var activePointer = null;
  var lastPlace = { x: 0, y: 0 };
  var lastMoveAt = 0;
  var moveTimer = 0;
  var pendingMove = null;

  // Pointer events carry the page's own clock: a touch's down and up can reach
  // the host in one batch behind a slow gesture, and only the page knows how long
  // the finger was held.
  function sendMove(place, buttons) {
    var event = { t: 'pointer', phase: 'move', x: place.x, y: place.y, buttons: buttons, at: performance.now() };
    var wait = MOVE_MS - (performance.now() - lastMoveAt);
    if (wait <= 0 && !moveTimer) {
      lastMoveAt = performance.now();
      push(event);
      return;
    }
    pendingMove = event;
    if (!moveTimer) moveTimer = setTimeout(flushMove, Math.max(wait, 0));
  }

  function flushMove() {
    moveTimer = 0;
    if (!pendingMove) return;
    lastMoveAt = performance.now();
    push(pendingMove);
    pendingMove = null;
  }

  function dropPendingMove() {
    clearTimeout(moveTimer);
    moveTimer = 0;
    pendingMove = null;
  }

  function focusKeys() {
    if (overlayShown()) return;
    keys.focus({ preventScroll: true });
  }

  picture.addEventListener('pointerdown', function (event) {
    focusKeys();
    if (activePointer !== null || !canInput()) return;
    var place = locate(event, false);
    if (!place) return;
    event.preventDefault();
    activePointer = event.pointerId;
    lastPlace = place;
    try { picture.setPointerCapture(event.pointerId); } catch (error) { /* the pointer is already gone */ }
    // An input method's window opens at the keyboard element.
    var box = stage.getBoundingClientRect();
    keys.style.left = Math.max(0, event.clientX - box.left) + 'px';
    keys.style.top = Math.max(0, event.clientY - box.top) + 'px';
    dropPendingMove();
    push({ t: 'pointer', phase: 'down', x: place.x, y: place.y, buttons: event.buttons || 1, at: performance.now() });
  });

  picture.addEventListener('pointermove', function (event) {
    if (activePointer !== null) {
      if (event.pointerId !== activePointer) return;
      var held = locate(event, true);
      if (!held) return;
      lastPlace = held;
      sendMove(held, event.buttons);
    } else if (event.pointerType === 'mouse' && state && state.platform === 'desktop' && canInput()) {
      // A desktop has a pointer that hovers.
      var over = locate(event, false);
      if (over) sendMove(over, 0);
    }
  });

  function endPointer(event, cancelled) {
    if (event.pointerId !== activePointer) return;
    var place = locate(event, true) || lastPlace;
    activePointer = null;
    dropPendingMove();
    push({ t: 'pointer', phase: cancelled ? 'cancel' : 'up', x: place.x, y: place.y, buttons: event.buttons, at: performance.now() });
  }

  picture.addEventListener('pointerup', function (event) { endPointer(event, false); });
  picture.addEventListener('pointercancel', function (event) { endPointer(event, true); });
  picture.addEventListener('lostpointercapture', function (event) { endPointer(event, true); });
  picture.addEventListener('contextmenu', function (event) { event.preventDefault(); });
  picture.addEventListener('dragstart', function (event) { event.preventDefault(); });

  picture.addEventListener('wheel', function (event) {
    event.preventDefault();
    if (!canInput()) return;
    var place = locate(event, false);
    if (!place) return;
    // Notches: a pixel wheel reports about a hundred a notch, a line wheel a few lines.
    var unit = event.deltaMode === 1 ? 1 / 3 : event.deltaMode === 2 ? 5 : 1 / 100;
    var dx = clampNotches(event.deltaX * unit);
    var dy = clampNotches(event.deltaY * unit);
    if (dx === 0 && dy === 0) return;
    push({ t: 'wheel', x: place.x, y: place.y, dx: dx, dy: dy });
  }, { passive: false });

  // ------------------------------------------------------------- keyboard

  // Keys held down, so a lost focus can let them go.
  var pressed = {};
  var composing = false;

  function pasteChord(event) {
    var key = (event.key || '').toLowerCase();
    return ((event.ctrlKey || event.metaKey) && !event.altKey && key === 'v') ||
      (event.shiftKey && !event.ctrlKey && !event.metaKey && key === 'insert');
  }

  function identity(event) {
    return event.code || event.key;
  }

  keys.addEventListener('keydown', function (event) {
    if (event.isComposing || event.keyCode === 229 || composing) return;
    if (!canInput() || pasteChord(event)) return;
    event.preventDefault();
    pressed[identity(event)] = { key: event.key, code: event.code || '' };
    // AltGr makes the key a character of the layout ('@' on a German
    // keyboard), not a shortcut. Windows also reports it as a Control press,
    // which the host would otherwise take for Ctrl held.
    var altGraph = typeof event.getModifierState === 'function' && event.getModifierState('AltGraph');
    push({ t: 'key', key: event.key, code: event.code || '', down: true, altGraph: altGraph === true });
  });

  keys.addEventListener('keyup', function (event) {
    if (event.isComposing || event.keyCode === 229 || composing) return;
    var held = pressed[identity(event)];
    // Only a key whose press was sent is released.
    if (!held) return;
    event.preventDefault();
    delete pressed[identity(event)];
    push({ t: 'key', key: held.key, code: held.code, down: false });
  });

  function releaseAll() {
    Object.keys(pressed).forEach(function (id) {
      var held = pressed[id];
      push({ t: 'key', key: held.key, code: held.code, down: false });
    });
    pressed = {};
    if (activePointer !== null) {
      dropPendingMove();
      push({ t: 'pointer', phase: 'cancel', x: lastPlace.x, y: lastPlace.y, buttons: 0, at: performance.now() });
      activePointer = null;
    }
  }

  keys.addEventListener('blur', releaseAll);
  window.addEventListener('blur', releaseAll);

  keys.addEventListener('compositionstart', function () { composing = true; });
  keys.addEventListener('compositionend', function (event) {
    composing = false;
    keys.value = '';
    if (event.data && canInput()) push({ t: 'text', text: event.data });
  });
  // Text that arrives without key presses (a phone's keyboard, dictation) is
  // typed as text. Composition and paste have their own events.
  keys.addEventListener('input', function (event) {
    if (composing || event.isComposing) return;
    keys.value = '';
    if (event.inputType === 'insertText' && event.data && canInput()) {
      push({ t: 'text', text: event.data });
    }
  });
  keys.addEventListener('paste', function (event) {
    event.preventDefault();
    var text = event.clipboardData ? event.clipboardData.getData('text/plain') : '';
    if (text && canInput()) push({ t: 'text', text: text.slice(0, PASTE_MAX) });
  });

  stage.addEventListener('click', function () { focusKeys(); });

  // ----------------------------------------------------------------- start

  render();
  poll();
  setInterval(poll, POLL_MS);
})();
