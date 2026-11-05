// What the WKWebView page engine runs in every document, before the document's own scripts.
//
// WebKit has no DevTools channel an application can open, so what Chromium reports over the protocol
// (network activity, the accessibility tree, DOM handles, the inspector overlay) is observed here and
// handed to `cdp.rs`. Everything this reports came from the page's own JavaScript world, so it is
// only as trustworthy as the page: a page can lie about its own network log, and nothing here can
// reach further than that. The message handler's name is random per page and kept in this closure.
(function () {
  "use strict";
  if (Object.prototype.hasOwnProperty.call(window, "__mewrkWk")) return;

  const handlers = window.webkit && window.webkit.messageHandlers;
  const handler = handlers && handlers["__MEWRK_HANDLER__"];
  const stringify = JSON.stringify;
  const post = (method, params) => {
    if (!handler) return;
    try {
      handler.postMessage(stringify({ method, params }));
    } catch (_) {}
  };
  const isMain = window.top === window;
  const nonce = Math.random().toString(36).slice(2, 8);
  const frameId = isMain ? "wk-main" : "wk-sub-" + nonce;

  // ----- handles ---------------------------------------------------------------------------------

  // Seeded per document so an id the host kept from a previous document can never name a node here.
  let nextNode = (Math.floor(Math.random() * 1e6) + 1) * 1000;
  const nodeIds = new WeakMap();
  const nodesById = new Map();
  const nodeId = (node) => {
    let id = nodeIds.get(node);
    if (id === undefined) {
      id = ++nextNode;
      nodeIds.set(node, id);
      nodesById.set(id, new WeakRef(node));
    }
    return id;
  };
  const nodeById = (id) => {
    const ref = nodesById.get(Number(id));
    const node = ref && ref.deref();
    if (!node) throw new Error("Could not find node with given id");
    return node;
  };
  let nextObject = 0;
  const objects = new Map();
  const objectIdOf = (value) => {
    if (value instanceof Node) return "n" + nodeId(value);
    const id = "o" + ++nextObject;
    objects.set(id, value);
    return id;
  };
  const object = (id) => {
    if (typeof id === "string" && id.startsWith("n")) return nodeById(id.slice(1));
    if (!objects.has(id)) throw new Error("Could not find object with given id");
    return objects.get(id);
  };
  const release = (id) => {
    objects.delete(id);
  };

  // What `Runtime.evaluate` calls a value.
  const remote = (value, byValue) => {
    const type = typeof value;
    if (value === undefined) return { type: "undefined" };
    if (type === "bigint") return { type, unserializableValue: String(value) + "n" };
    if (type === "number" && !Number.isFinite(value)) return { type, unserializableValue: String(value) };
    if (value === null) return { type: "object", subtype: "null", value: null };
    if (type !== "object" && type !== "function") return { type, value };
    if (byValue) return { type, value };
    const isNode = value instanceof Node;
    return {
      type,
      subtype: isNode ? "node" : Array.isArray(value) ? "array" : undefined,
      className: value.constructor && value.constructor.name,
      description: isNode ? value.nodeName : undefined,
      objectId: objectIdOf(value),
    };
  };
  const encode = (value, byValue) => {
    try {
      return stringify({ result: remote(value, byValue) });
    } catch (error) {
      // A cycle cannot go by value; Chromium refuses it the same way.
      return encodeError(error);
    }
  };
  const encodeError = (error) => {
    const name = error && error.name ? String(error.name) : "Error";
    const message = error && error.message !== undefined ? String(error.message) : String(error);
    const stack = error && error.stack ? String(error.stack) : "";
    const description = name + ": " + message + (stack ? "\n" + stack : "");
    return stringify({
      result: { type: "object", subtype: "error", className: name, description },
      exceptionDetails: {
        text: "Uncaught",
        exception: { type: "object", subtype: "error", className: name, description },
      },
    });
  };

  // ----- network ---------------------------------------------------------------------------------

  let requestSeq = 0;
  const nextRequest = () => nonce + "." + ++requestSeq;
  const begin = (requestId, url, method, type) =>
    post("Network.requestWillBeSent", { requestId, request: { url, method }, type, frameId });
  const respond = (requestId, url, status, statusText, type) =>
    post("Network.responseReceived", { requestId, type, frameId, response: { url, status, statusText } });
  const finish = (requestId) => post("Network.loadingFinished", { requestId });
  const fail = (requestId, errorText, type) => post("Network.loadingFailed", { requestId, errorText, type });

  // What `Network.getResponseBody` can answer: the bodies the page itself asked for, newest first.
  const BODY_LIMIT = 1 << 20;
  const BODIES_KEPT = 64;
  const bodies = new Map();
  const keepBody = (requestId, body, base64Encoded) => {
    bodies.set(requestId, { body, base64Encoded });
    if (bodies.size > BODIES_KEPT) bodies.delete(bodies.keys().next().value);
  };
  const isTextType = (type) =>
    !type || /^text\/|json|xml|javascript|ecmascript|x-www-form-urlencoded|graphql/i.test(type);
  const toBase64 = (bytes) => {
    let binary = "";
    for (let i = 0; i < bytes.length; i += 0x8000) {
      binary += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    }
    return btoa(binary);
  };
  // Reads a copy to its end. A stream that never ends never finishes, as in Chromium, and costs at
  // most the limit: past it the copy is cancelled rather than letting the tee buffer without bound.
  const drainBody = async (requestId, response) => {
    const type = response.headers.get("content-type");
    const reader = response.body && response.body.getReader();
    if (!reader) {
      finish(requestId);
      return;
    }
    const chunks = [];
    let total = 0;
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      total += value.length;
      if (total > BODY_LIMIT) {
        reader.cancel().catch(() => {});
        finish(requestId);
        return;
      }
      chunks.push(value);
    }
    const bytes = new Uint8Array(total);
    let offset = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, offset);
      offset += chunk.length;
    }
    if (isTextType(type)) keepBody(requestId, new TextDecoder().decode(bytes), false);
    else keepBody(requestId, toBase64(bytes), true);
    finish(requestId);
  };

  // A page on another machine's network: WebKit won't proxy a loopback address, so what the page's
  // script asks of one goes by its alias (see `LOOPBACK_ALIASES` in `wk/mod.rs`).
  let loopbackAliased = false;
  const LOOPBACK = {
    localhost: "machine.localhost",
    "127.0.0.1": "ipv4.machine.localhost",
    "0.0.0.0": "ipv4.machine.localhost",
    "[::1]": "ipv6.machine.localhost",
  };
  const onMachine = (raw) => {
    if (!loopbackAliased) return raw;
    try {
      const url = new URL(String(raw), location.href);
      const alias = LOOPBACK[url.hostname];
      if (!alias) return raw;
      url.hostname = alias;
      return url.href;
    } catch (_) {
      return raw;
    }
  };
  for (const name of ["WebSocket", "EventSource"]) {
    const Native = window[name];
    if (!Native) continue;
    window[name] = new Proxy(Native, {
      construct(target, args, newTarget) {
        if (args.length) args[0] = onMachine(args[0]);
        return Reflect.construct(target, args, newTarget);
      },
    });
  }

  const nativeFetch = window.fetch;
  if (nativeFetch) {
    window.fetch = function fetch(input, init) {
      const requestId = nextRequest();
      if (loopbackAliased) {
        if (typeof Request !== "undefined" && input instanceof Request) {
          const moved = onMachine(input.url);
          if (moved !== input.url) arguments[0] = input = new Request(moved, input);
        } else {
          arguments[0] = input = onMachine(input);
        }
      }
      let url = "";
      let method = "GET";
      try {
        const request = typeof Request !== "undefined" && input instanceof Request ? input : null;
        url = new URL(request ? request.url : String(input), location.href).href;
        method = String((init && init.method) || (request && request.method) || "GET").toUpperCase();
      } catch (_) {
        url = String(input);
      }
      begin(requestId, url, method, "Fetch");
      return nativeFetch.apply(this, arguments).then(
        (response) => {
          respond(requestId, response.url || url, response.status, response.statusText, "Fetch");
          try {
            drainBody(requestId, response.clone()).catch(() => finish(requestId));
          } catch (_) {
            finish(requestId);
          }
          return response;
        },
        (error) => {
          fail(requestId, String((error && error.message) || error), "Fetch");
          throw error;
        }
      );
    };
  }

  if (window.XMLHttpRequest) {
    const open = XMLHttpRequest.prototype.open;
    const send = XMLHttpRequest.prototype.send;
    const meta = new WeakMap();
    XMLHttpRequest.prototype.open = function (method, url) {
      arguments[1] = url = onMachine(url);
      meta.set(this, { method: String(method).toUpperCase(), url: String(url) });
      return open.apply(this, arguments);
    };
    XMLHttpRequest.prototype.send = function () {
      const info = meta.get(this);
      if (info) {
        const requestId = nextRequest();
        let url = info.url;
        try {
          url = new URL(info.url, location.href).href;
        } catch (_) {}
        begin(requestId, url, info.method, "XHR");
        this.addEventListener(
          "loadend",
          () => {
            if (this.status === 0) {
              fail(requestId, "net::ERR_FAILED", "XHR");
              return;
            }
            respond(requestId, this.responseURL || url, this.status, this.statusText, "XHR");
            try {
              if (this.responseType === "" || this.responseType === "text") {
                if (this.responseText.length <= BODY_LIMIT) keepBody(requestId, this.responseText, false);
              }
            } catch (_) {}
            finish(requestId);
          },
          { once: true }
        );
      }
      return send.apply(this, arguments);
    };
  }

  const resourceType = (initiator) => {
    switch (initiator) {
      case "img":
      case "image":
      case "input":
        return "Image";
      case "script":
        return "Script";
      case "link":
      case "css":
        return "Stylesheet";
      case "video":
      case "audio":
        return "Media";
      case "iframe":
      case "frame":
        return "Document";
      default:
        return "Other";
    }
  };
  // Subresources are only known once they are done: WebKit tells a page nothing earlier.
  try {
    new PerformanceObserver((list) => {
      for (const entry of list.getEntries()) {
        const initiator = entry.initiatorType;
        if (initiator === "fetch" || initiator === "xmlhttprequest" || initiator === "beacon") continue;
        const requestId = nextRequest();
        const type = resourceType(initiator);
        begin(requestId, entry.name, "GET", type);
        respond(requestId, entry.name, entry.responseStatus || 200, "", type);
        finish(requestId);
      }
    }).observe({ type: "resource", buffered: true });
  } catch (_) {}
  // The one way a failed subresource shows at all.
  addEventListener(
    "error",
    (event) => {
      const target = event.target;
      if (!target || target === window || !(target instanceof Element)) return;
      const url = target.src || target.href;
      if (!url) return;
      const requestId = nextRequest();
      const type = resourceType(target.localName);
      begin(requestId, String(url), "GET", type);
      fail(requestId, "net::ERR_FAILED", type);
    },
    true
  );

  if (isMain) {
    document.addEventListener(
      "DOMContentLoaded",
      () => post("Page.domContentEventFired", { timestamp: performance.now() / 1000 }),
      { once: true }
    );
  }

  // ----- input -----------------------------------------------------------------------------------

  // Whether a synthesized NSEvent actually arrived is only knowable from here. `pointerup` as well as
  // `mouseup`, because a page that cancels `pointerdown` (the picker does) never gets the mouse events.
  let trustedMouseUps = 0;
  let lastFileInput = null;
  const countMouseUp = (event) => {
    if (event.isTrusted) trustedMouseUps += 1;
  };
  addEventListener("pointerup", countMouseUp, true);
  addEventListener("mouseup", countMouseUp, true);
  addEventListener(
    "click",
    (event) => {
      const target = event.composedPath()[0];
      if (target instanceof HTMLInputElement && target.type === "file") lastFileInput = target;
    },
    true
  );

  // The fallback when the view could not take a real event: the same sequence as script events, so
  // handlers run, with `isTrusted: false` and without the browser's own default actions.
  const syntheticClick = (x, y, clickCount) => {
    const target = document.elementFromPoint(x, y) || document.documentElement;
    const init = {
      bubbles: true,
      cancelable: true,
      composed: true,
      clientX: x,
      clientY: y,
      button: 0,
      detail: clickCount || 1,
      view: window,
    };
    const fire = (Type, type, buttons) => target.dispatchEvent(new Type(type, { ...init, buttons }));
    fire(PointerEvent, "pointerdown", 1);
    fire(MouseEvent, "mousedown", 1);
    if (typeof target.focus === "function") target.focus();
    fire(PointerEvent, "pointerup", 0);
    fire(MouseEvent, "mouseup", 0);
    if (typeof target.click === "function") target.click();
    else fire(MouseEvent, "click", 0);
    return true;
  };

  // ----- DOM -------------------------------------------------------------------------------------

  const describe = (id, depth) => {
    const node = nodeById(id);
    const attributes = [];
    if (node.attributes) {
      for (const attribute of node.attributes) attributes.push(attribute.name, attribute.value);
    }
    const out = {
      nodeId: Number(id),
      backendNodeId: Number(id),
      nodeType: node.nodeType,
      nodeName: node.nodeName,
      localName: node.localName || "",
      nodeValue: node.nodeValue || "",
      childNodeCount: node.childNodes ? node.childNodes.length : 0,
      frameId,
    };
    if (node.attributes) out.attributes = attributes;
    if (node.nodeType === Node.DOCUMENT_NODE) {
      out.documentURL = document.URL;
      out.baseURL = document.baseURI;
    }
    if (depth > 0 && node.childNodes) {
      out.children = Array.from(node.childNodes, (child) => describe(nodeId(child), depth - 1));
    }
    return out;
  };

  const computedStyle = (id) => {
    const node = nodeById(id);
    const element = node instanceof Element ? node : node.parentElement;
    if (!element) throw new Error("Node is not an element");
    const style = getComputedStyle(element);
    const out = [];
    for (let i = 0; i < style.length; i += 1) {
      const name = style[i];
      out.push({ name, value: style.getPropertyValue(name) });
    }
    return { computedStyle: out };
  };

  const quad = (left, top, right, bottom) => [left, top, right, top, right, bottom, left, bottom];
  const boxModel = (id) => {
    const node = nodeById(id);
    const element = node instanceof Element ? node : node.parentElement;
    if (!element) throw new Error("Could not compute box model.");
    const rect = element.getBoundingClientRect();
    const style = getComputedStyle(element);
    const px = (name) => parseFloat(style.getPropertyValue(name)) || 0;
    const border = [px("border-top-width"), px("border-right-width"), px("border-bottom-width"), px("border-left-width")];
    const padding = [px("padding-top"), px("padding-right"), px("padding-bottom"), px("padding-left")];
    const margin = [px("margin-top"), px("margin-right"), px("margin-bottom"), px("margin-left")];
    return {
      model: {
        content: quad(
          rect.left + border[3] + padding[3],
          rect.top + border[0] + padding[0],
          rect.right - border[1] - padding[1],
          rect.bottom - border[2] - padding[2]
        ),
        padding: quad(rect.left + border[3], rect.top + border[0], rect.right - border[1], rect.bottom - border[2]),
        border: quad(rect.left, rect.top, rect.right, rect.bottom),
        margin: quad(rect.left - margin[3], rect.top - margin[0], rect.right + margin[1], rect.bottom + margin[2]),
        width: rect.width,
        height: rect.height,
      },
    };
  };

  const querySelector = (id, selector) => {
    const root = nodeById(id);
    const found = root.querySelector(selector);
    return { nodeId: found ? nodeId(found) : 0 };
  };

  const pushNodes = (ids) => ({
    nodeIds: ids.map((id) => {
      try {
        nodeById(id);
        return Number(id);
      } catch (_) {
        return 0;
      }
    }),
  });

  // A chooser nobody opened: a file input takes a `FileList` only from a `DataTransfer`.
  const setFiles = (id, files) => {
    const input = object(id);
    if (!(input instanceof HTMLInputElement) || input.type !== "file") {
      throw new Error("Node is not a file input element");
    }
    const transfer = new DataTransfer();
    for (const file of files) {
      const binary = atob(file.data);
      const bytes = new Uint8Array(binary.length);
      for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
      transfer.items.add(new File([bytes], file.name, { type: file.type || "", lastModified: file.lastModified || Date.now() }));
    }
    input.files = transfer.files;
    input.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
    input.dispatchEvent(new Event("change", { bubbles: true }));
    return {};
  };

  const layoutMetrics = () => {
    const viewport = window.visualViewport;
    const root = document.documentElement;
    // The real ratio even under emulation: this is what a capture's size is worked out from.
    const emulated = saved.get("w.devicePixelRatio");
    const scale = (emulated && emulated[2] && emulated[2].get ? emulated[2].get.call(window) : window.devicePixelRatio) || 1;
    const width = viewport ? viewport.width : innerWidth;
    const height = viewport ? viewport.height : innerHeight;
    const contentWidth = Math.max(root ? root.scrollWidth : 0, document.body ? document.body.scrollWidth : 0, width);
    const contentHeight = Math.max(root ? root.scrollHeight : 0, document.body ? document.body.scrollHeight : 0, height);
    const css = {
      offsetX: 0,
      offsetY: 0,
      pageX: viewport ? viewport.pageLeft : scrollX,
      pageY: viewport ? viewport.pageTop : scrollY,
      clientWidth: width,
      clientHeight: height,
      scale: viewport ? viewport.scale : 1,
      zoom: 1,
    };
    return {
      cssLayoutViewport: { pageX: scrollX, pageY: scrollY, clientWidth: innerWidth, clientHeight: innerHeight },
      layoutViewport: { pageX: scrollX, pageY: scrollY, clientWidth: innerWidth, clientHeight: innerHeight },
      cssVisualViewport: css,
      visualViewport: { ...css, clientWidth: width * scale, clientHeight: height * scale },
      cssContentSize: { x: 0, y: 0, width: contentWidth, height: contentHeight },
      contentSize: { x: 0, y: 0, width: contentWidth * scale, height: contentHeight * scale },
    };
  };

  // ----- accessibility tree ----------------------------------------------------------------------

  // WebKit exposes its accessibility tree only to the system's assistive clients, so this computes
  // the roles and names ARIA-in-HTML and the accessible-name algorithm define, in the shape of
  // `Accessibility.getFullAXTree`.
  const AX_NODE_LIMIT = 20000;
  const implicitRole = (element) => {
    const tag = element.localName;
    switch (tag) {
      case "a":
      case "area":
        return element.hasAttribute("href") ? "link" : "generic";
      case "article":
        return "article";
      case "aside":
        return "complementary";
      case "button":
        return "button";
      case "dialog":
        return "dialog";
      case "fieldset":
        return "group";
      case "figure":
        return "figure";
      case "footer":
        return element.closest("article,aside,main,nav,section") ? "generic" : "contentinfo";
      case "form":
        return "form";
      case "h1":
      case "h2":
      case "h3":
      case "h4":
      case "h5":
      case "h6":
        return "heading";
      case "header":
        return element.closest("article,aside,main,nav,section") ? "generic" : "banner";
      case "hr":
        return "separator";
      case "img":
        return element.getAttribute("alt") === "" ? "none" : "img";
      case "input": {
        switch ((element.getAttribute("type") || "text").toLowerCase()) {
          case "button":
          case "image":
          case "reset":
          case "submit":
            return "button";
          case "checkbox":
            return "checkbox";
          case "radio":
            return "radio";
          case "range":
            return "slider";
          case "number":
            return "spinbutton";
          case "search":
            return "searchbox";
          case "hidden":
            return null;
          default:
            return element.hasAttribute("list") ? "combobox" : "textbox";
        }
      }
      case "li":
        return "listitem";
      case "main":
        return "main";
      case "math":
        return "math";
      case "menu":
      case "ol":
      case "ul":
        return "list";
      case "meter":
        return "meter";
      case "nav":
        return "navigation";
      case "option":
        return "option";
      case "p":
        return "paragraph";
      case "progress":
        return "progressbar";
      case "section":
        return element.hasAttribute("aria-label") || element.hasAttribute("aria-labelledby") ? "region" : "generic";
      case "select":
        return element.multiple || element.size > 1 ? "listbox" : "combobox";
      case "svg":
        return "SvgRoot";
      case "table":
        return "table";
      case "tbody":
      case "tfoot":
      case "thead":
        return "rowgroup";
      case "td":
        return "cell";
      case "textarea":
        return "textbox";
      case "th":
        return element.getAttribute("scope") === "row" ? "rowheader" : "columnheader";
      case "tr":
        return "row";
      case "label":
        return "LabelText";
      case "summary":
        return "DisclosureTriangle";
      case "script":
      case "style":
      case "noscript":
      case "template":
      case "head":
      case "meta":
      case "link":
      case "title":
        return null;
      default:
        return "generic";
    }
  };
  const NAME_FROM_CONTENT = new Set([
    "button", "cell", "checkbox", "columnheader", "DisclosureTriangle", "heading", "link", "menuitem",
    "option", "radio", "row", "rowheader", "switch", "tab", "tooltip", "treeitem", "LabelText",
  ]);
  const normalize = (text) => String(text || "").replace(/\s+/g, " ").trim();
  const textOf = (element) => normalize(element.innerText !== undefined ? element.innerText : element.textContent);
  const axName = (element, role) => {
    const labelledBy = element.getAttribute("aria-labelledby");
    if (labelledBy) {
      const root = element.getRootNode();
      const parts = labelledBy
        .split(/\s+/)
        .map((id) => (root.getElementById ? root.getElementById(id) : null))
        .filter(Boolean)
        .map(textOf);
      if (parts.length) return parts.join(" ");
    }
    const ariaLabel = normalize(element.getAttribute("aria-label"));
    if (ariaLabel) return ariaLabel;
    if (element.localName === "img" || element.localName === "area") {
      const alt = normalize(element.getAttribute("alt"));
      if (alt) return alt;
    }
    if (element.labels && element.labels.length) {
      const labels = Array.from(element.labels, textOf).filter(Boolean);
      if (labels.length) return labels.join(" ");
    }
    if (element.localName === "input") {
      const type = (element.getAttribute("type") || "text").toLowerCase();
      if (type === "submit" || type === "button" || type === "reset") {
        const value = normalize(element.value);
        if (value) return value;
      }
      if (type === "image") {
        const alt = normalize(element.getAttribute("alt"));
        if (alt) return alt;
      }
    }
    if (role === "dialog" || role === "group" || role === "table" || role === "figure") {
      const caption = element.querySelector(":scope > legend, :scope > caption, :scope > figcaption");
      if (caption) return textOf(caption);
    }
    if (NAME_FROM_CONTENT.has(role)) {
      const text = textOf(element);
      if (text) return text;
    }
    const placeholder = normalize(element.getAttribute("placeholder"));
    if (placeholder) return placeholder;
    return normalize(element.getAttribute("title"));
  };
  const axHidden = (element) => {
    if (element.hidden || element.getAttribute("aria-hidden") === "true") return true;
    const style = getComputedStyle(element);
    return style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse";
  };
  const axTree = () => {
    const nodes = [];
    const add = (role, name, node, extra) => {
      const entry = {
        nodeId: String(nodes.length + 1),
        ignored: false,
        role: { type: "role", value: role },
        name: { type: "computedString", value: name || "" },
        childIds: [],
        backendDOMNodeId: nodeId(node),
        ...extra,
      };
      nodes.push(entry);
      return entry;
    };
    const walkChildren = (parent, into) => {
      const shadow = parent.shadowRoot;
      for (const child of (shadow || parent).childNodes) {
        if (nodes.length >= AX_NODE_LIMIT) return;
        walk(child, into);
      }
    };
    const walk = (node, into) => {
      if (node.nodeType === Node.TEXT_NODE) {
        const text = normalize(node.nodeValue);
        if (!text) return;
        const entry = add("StaticText", text, node);
        into.childIds.push(entry.nodeId);
        return;
      }
      if (node.nodeType !== Node.ELEMENT_NODE) return;
      const element = node;
      if (axHidden(element)) return;
      const explicit = normalize(element.getAttribute("role")).split(" ")[0];
      const role = explicit || implicitRole(element);
      if (!role) return;
      const name = axName(element, role);
      const description = normalize(element.getAttribute("aria-describedby") ? "" : element.getAttribute("aria-description"));
      const extra = {};
      if (description) extra.description = { type: "computedString", value: description };
      if ("value" in element && (role === "textbox" || role === "searchbox" || role === "combobox" || role === "spinbutton" || role === "slider")) {
        const value = element.localName === "input" && element.type === "password" ? "" : String(element.value || "");
        if (value) extra.value = { type: "string", value };
      } else if (element.getAttribute("aria-valuenow")) {
        extra.value = { type: "string", value: element.getAttribute("aria-valuenow") };
      }
      const entry = add(role, name, element, extra);
      into.childIds.push(entry.nodeId);
      if (role === "SvgRoot") return;
      walkChildren(element, entry);
    };
    const root = add("RootWebArea", normalize(document.title), document);
    if (document.documentElement) {
      const body = document.body || document.documentElement;
      walkChildren(body, root);
    }
    return { nodes };
  };

  // ----- element picker --------------------------------------------------------------------------

  let picker = null;
  const paint = (color, fallback) =>
    color ? "rgba(" + color.r + "," + color.g + "," + color.b + "," + (color.a === undefined ? 1 : color.a) + ")" : fallback;
  const hideHighlight = () => {
    if (picker) picker.box.style.display = "none";
    if (picker) picker.tip.style.display = "none";
    return {};
  };
  const pickerDisarm = () => {
    if (!picker) return {};
    for (const [type, listener] of picker.listeners) removeEventListener(type, listener, true);
    picker.host.remove();
    picker = null;
    return {};
  };
  const pickerArm = (config) => {
    pickerDisarm();
    const parent = document.documentElement;
    if (!parent) return {};
    const host = document.createElement("div");
    host.setAttribute("data-mewrk-inspect", "");
    host.style.cssText = "all:initial;position:fixed;inset:0;z-index:2147483647;pointer-events:none;";
    const shadow = host.attachShadow({ mode: "closed" });
    const box = document.createElement("div");
    box.style.cssText =
      "position:fixed;display:none;box-sizing:border-box;pointer-events:none;background:" +
      paint(config && config.contentColor, "rgba(111,168,220,.66)") +
      ";outline:1px solid " +
      paint(config && config.borderColor, "rgba(255,229,153,.66)") +
      ";";
    const tip = document.createElement("div");
    tip.style.cssText =
      "position:fixed;display:none;pointer-events:none;font:12px/1.4 -apple-system,sans-serif;color:#fff;background:#1f2937;padding:2px 6px;border-radius:3px;white-space:nowrap;";
    shadow.append(box, tip);
    parent.appendChild(host);
    const elementAt = (event) => {
      const path = event.composedPath();
      const first = path[0];
      return first instanceof Element ? first : document.elementFromPoint(event.clientX, event.clientY);
    };
    const onMove = (event) => {
      if (!event.isTrusted) return;
      const element = elementAt(event);
      if (!element) return;
      const rect = element.getBoundingClientRect();
      box.style.display = "block";
      box.style.left = rect.left + "px";
      box.style.top = rect.top + "px";
      box.style.width = rect.width + "px";
      box.style.height = rect.height + "px";
      tip.style.display = "block";
      tip.textContent = element.localName + (element.id ? "#" + element.id : "") + " " + Math.round(rect.width) + "×" + Math.round(rect.height);
      tip.style.left = Math.max(0, rect.left) + "px";
      tip.style.top = (rect.top > 24 ? rect.top - 22 : rect.bottom + 2) + "px";
    };
    const swallow = (event) => {
      if (!event.isTrusted) return;
      event.preventDefault();
      event.stopImmediatePropagation();
    };
    const onClick = (event) => {
      if (!event.isTrusted) return;
      swallow(event);
      const element = elementAt(event);
      if (element) post("Overlay.inspectNodeRequested", { backendNodeId: nodeId(element) });
    };
    const onKey = (event) => {
      if (!event.isTrusted || event.key !== "Escape") return;
      swallow(event);
      post("Overlay.inspectModeCanceled", {});
    };
    const listeners = [
      ["mousemove", onMove],
      ["pointerdown", swallow],
      ["mousedown", swallow],
      ["pointerup", swallow],
      ["mouseup", swallow],
      ["dblclick", swallow],
      ["auxclick", swallow],
      ["click", onClick],
      ["keydown", onKey],
    ];
    for (const [type, listener] of listeners) addEventListener(type, listener, true);
    picker = { host, box, tip, listeners };
    return {};
  };

  // ----- emulation -------------------------------------------------------------------------------

  // What `WKWebView` has no setting for, stated to the page's own script. Each override is taken
  // back by putting the descriptor it replaced back in place.
  const saved = new Map();
  const override = (target, key, value) => {
    const id = target === window ? "w." + key : target === navigator ? "n." + key : "s." + key;
    if (!saved.has(id)) saved.set(id, [target, key, Object.getOwnPropertyDescriptor(target, key)]);
    try {
      Object.defineProperty(target, key, { configurable: true, get: () => value });
    } catch (_) {}
  };
  const restore = (target, key) => {
    const id = target === window ? "w." + key : target === navigator ? "n." + key : "s." + key;
    const entry = saved.get(id);
    if (!entry) return;
    saved.delete(id);
    try {
      if (entry[2]) Object.defineProperty(target, key, entry[2]);
      else delete target[key];
    } catch (_) {}
  };
  let touchTranslate = null;
  const translateTouch = (on) => {
    if (touchTranslate) {
      for (const [type, listener] of touchTranslate) removeEventListener(type, listener, true);
      touchTranslate = null;
    }
    if (!on) return;
    // `TouchEvent` does not exist on the Mac, so a plain event carries what handlers read off one.
    const send = (type) => (event) => {
      if (!event.isTrusted) return;
      const point = { identifier: 0, target: event.target, clientX: event.clientX, clientY: event.clientY, pageX: event.pageX, pageY: event.pageY, screenX: event.screenX, screenY: event.screenY };
      const touch = new Event(type, { bubbles: true, cancelable: true, composed: true });
      const list = type === "touchend" ? [] : [point];
      Object.assign(touch, { touches: list, targetTouches: list, changedTouches: [point] });
      event.target.dispatchEvent(touch);
    };
    touchTranslate = [
      ["mousedown", send("touchstart")],
      ["mouseup", send("touchend")],
    ];
    for (const [type, listener] of touchTranslate) addEventListener(type, listener, true);
  };
  const emulate = (config) => {
    if (config.deviceScaleFactor) {
      override(window, "devicePixelRatio", config.deviceScaleFactor);
    } else {
      restore(window, "devicePixelRatio");
    }
    if (config.screenWidth && config.screenHeight) {
      override(screen, "width", config.screenWidth);
      override(screen, "height", config.screenHeight);
      override(screen, "availWidth", config.screenWidth);
      override(screen, "availHeight", config.screenHeight);
    } else {
      for (const key of ["width", "height", "availWidth", "availHeight"]) restore(screen, key);
    }
    if (config.maxTouchPoints) {
      override(navigator, "maxTouchPoints", config.maxTouchPoints);
      override(window, "ontouchstart", null);
    } else {
      restore(navigator, "maxTouchPoints");
      restore(window, "ontouchstart");
    }
    translateTouch(Boolean(config.emitTouchEvents));
    return {};
  };

  // ----- what `cdp.rs` calls -----------------------------------------------------------------------

  Object.defineProperty(window, "__mewrkWk", {
    configurable: false,
    enumerable: false,
    writable: false,
    value: Object.freeze({
      encode,
      encodeError,
      object,
      release,
      nodeId,
      nodeById,
      objectIdOf,
      describe,
      computedStyle,
      boxModel,
      querySelector,
      pushNodes,
      setFiles,
      layoutMetrics,
      axTree,
      pickerArm,
      pickerDisarm,
      hideHighlight,
      emulate,
      syntheticClick,
      setLoopbackAliased: (on) => {
        loopbackAliased = Boolean(on);
        return {};
      },
      responseBody: (requestId) => {
        const body = bodies.get(requestId);
        if (!body) throw new Error("No data found for resource with given identifier");
        return body;
      },
      trustedMouseUps: () => trustedMouseUps,
      lastFileInput: () => (lastFileInput && lastFileInput.isConnected ? nodeId(lastFileInput) : null),
    }),
  });
})();
