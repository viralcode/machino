// Shared DOM host for machino WASM modules.
// mode: "browser" | "virtual"
// After instantiate, call host.bindExports(instance.exports) so event
// listeners can invoke exported zero-arg handlers by name.

export function createDomHost({ readStr, makeStr, mode = "virtual" }) {
  const nodes = new Map();
  let next = 2;
  let exports = null;
  let lastEventType = "";
  let lastEventTarget = 0;
  // key: `${elHandle}:${event}` -> handler name
  const listeners = new Map();

  nodes.set(1, {
    tag: "#document",
    text: "",
    attrs: {},
    styles: {},
    children: [],
    parent: 0,
    width: 800,
    height: 600,
    el: mode === "browser" ? document.documentElement : null,
  });

  function node(h) {
    return nodes.get(Number(h));
  }

  function htmlOf(h) {
    const n = node(h);
    if (!n) return "";
    if (n.tag === "#document") return n.children.map(htmlOf).join("");
    let s = `<${n.tag}`;
    for (const [k, v] of Object.entries(n.attrs)) s += ` ${k}="${v}"`;
    s += `>${n.text}`;
    for (const c of n.children) s += htmlOf(c);
    s += `</${n.tag}>`;
    return s;
  }

  function classList(n) {
    return (n.attrs.class ?? "").split(/\s+/).filter(Boolean);
  }

  function setClassList(n, parts) {
    n.attrs.class = parts.join(" ");
    if (n.el) n.el.className = n.attrs.class;
  }

  function fire(elH, event) {
    lastEventType = event;
    lastEventTarget = Number(elH);
    const key = `${Number(elH)}:${event}`;
    const handler = listeners.get(key);
    if (!handler || !exports) return;
    const fn = exports[handler];
    if (typeof fn === "function") {
      fn();
    } else {
      console.error(`dom listener: exported handler '${handler}' not found`);
    }
  }

  function ensureBrowserListener(n, elH, event) {
    if (mode !== "browser" || !n.el) return;
    if (n._bound && n._bound.has(event)) return;
    if (!n._bound) n._bound = new Set();
    n._bound.add(event);
    n.el.addEventListener(event, () => fire(elH, event));
  }

  const api = {
    bindExports(ex) {
      exports = ex;
    },

    dom_document() {
      return 1n;
    },

    dom_create_element(tagAddr) {
      const tag = readStr(tagAddr);
      const h = BigInt(next++);
      const el = mode === "browser" ? document.createElement(tag) : null;
      nodes.set(Number(h), {
        tag,
        text: "",
        attrs: {},
        styles: {},
        children: [],
        parent: 0,
        width: 100,
        height: 40,
        el,
        _bound: new Set(),
      });
      return h;
    },

    dom_get_element_by_id(idAddr) {
      const id = readStr(idAddr);
      if (mode === "browser") {
        const el = document.getElementById(id);
        if (!el) return 0n;
        for (const [h, n] of nodes) if (n.el === el) return BigInt(h);
        const h = BigInt(next++);
        nodes.set(Number(h), {
          tag: el.tagName.toLowerCase(),
          text: el.textContent ?? "",
          attrs: { id },
          styles: {},
          children: [],
          parent: 0,
          width: el.clientWidth || 0,
          height: el.clientHeight || 0,
          el,
          _bound: new Set(),
        });
        return h;
      }
      for (const [h, n] of nodes) if (n.attrs.id === id) return BigInt(h);
      return 0n;
    },

    dom_query_selector(selAddr) {
      const sel = readStr(selAddr);
      if (mode === "browser") {
        const el = document.querySelector(sel);
        if (!el) return 0n;
        for (const [h, n] of nodes) if (n.el === el) return BigInt(h);
        const h = BigInt(next++);
        nodes.set(Number(h), {
          tag: el.tagName.toLowerCase(),
          text: el.textContent ?? "",
          attrs: {},
          styles: {},
          children: [],
          parent: 0,
          width: el.clientWidth || 0,
          height: el.clientHeight || 0,
          el,
          _bound: new Set(),
        });
        return h;
      }
      if (sel.startsWith("#")) {
        const id = sel.slice(1);
        for (const [h, n] of nodes) if (n.attrs.id === id) return BigInt(h);
        return 0n;
      }
      for (const [h, n] of nodes) if (n.tag === sel) return BigInt(h);
      return 0n;
    },

    dom_set_text(h, textAddr) {
      const n = node(h);
      if (!n) return;
      n.text = readStr(textAddr);
      n.children = [];
      if (n.el) n.el.textContent = n.text;
    },
    dom_get_text(h) {
      const n = node(h);
      if (!n) return makeStr("");
      if (n.el) return makeStr(n.el.textContent ?? "");
      return makeStr(n.text);
    },
    dom_set_html(h, htmlAddr) {
      const n = node(h);
      if (!n) return;
      n.text = readStr(htmlAddr);
      n.children = [];
      if (n.el) n.el.innerHTML = n.text;
    },
    dom_get_html(h) {
      const n = node(h);
      if (!n) return makeStr("");
      if (n.el) return makeStr(n.el.innerHTML);
      return makeStr(htmlOf(h));
    },
    dom_set_attr(h, nameAddr, valueAddr) {
      const n = node(h);
      if (!n) return;
      const name = readStr(nameAddr);
      const value = readStr(valueAddr);
      n.attrs[name] = value;
      if (n.el) n.el.setAttribute(name, value);
    },
    dom_get_attr(h, nameAddr) {
      const n = node(h);
      if (!n) return makeStr("");
      const name = readStr(nameAddr);
      if (n.el) return makeStr(n.el.getAttribute(name) ?? "");
      return makeStr(n.attrs[name] ?? "");
    },
    dom_append_child(parentH, childH) {
      const p = node(parentH);
      const c = node(childH);
      if (!p || !c) return;
      if (c.parent) {
        const op = node(c.parent);
        if (op) op.children = op.children.filter((x) => x !== Number(childH));
      }
      c.parent = Number(parentH);
      if (!p.children.includes(Number(childH))) p.children.push(Number(childH));
      if (p.el && c.el) p.el.appendChild(c.el);
    },
    dom_remove_child(parentH, childH) {
      const p = node(parentH);
      const c = node(childH);
      if (!p || !c) return;
      p.children = p.children.filter((x) => x !== Number(childH));
      c.parent = 0;
      if (p.el && c.el && c.el.parentNode === p.el) p.el.removeChild(c.el);
    },
    dom_clear_children(h) {
      const n = node(h);
      if (!n) return;
      for (const c of [...n.children]) {
        const ch = node(c);
        if (ch) ch.parent = 0;
      }
      n.children = [];
      n.text = "";
      if (n.el) n.el.replaceChildren();
    },
    dom_add_class(h, clsAddr) {
      const n = node(h);
      if (!n) return;
      const cls = readStr(clsAddr);
      const parts = classList(n);
      if (!parts.includes(cls)) parts.push(cls);
      setClassList(n, parts);
      if (n.el) n.el.classList.add(cls);
    },
    dom_remove_class(h, clsAddr) {
      const n = node(h);
      if (!n) return;
      const cls = readStr(clsAddr);
      setClassList(
        n,
        classList(n).filter((c) => c !== cls)
      );
      if (n.el) n.el.classList.remove(cls);
    },
    dom_toggle_class(h, clsAddr) {
      const n = node(h);
      if (!n) return 0n;
      const cls = readStr(clsAddr);
      const parts = classList(n);
      const i = parts.indexOf(cls);
      if (i >= 0) parts.splice(i, 1);
      else parts.push(cls);
      setClassList(n, parts);
      if (n.el) n.el.classList.toggle(cls);
      return parts.includes(cls) ? 1n : 0n;
    },
    dom_has_class(h, clsAddr) {
      const n = node(h);
      if (!n) return 0n;
      return classList(n).includes(readStr(clsAddr)) ? 1n : 0n;
    },
    dom_set_style(h, propAddr, valueAddr) {
      const n = node(h);
      if (!n) return;
      const prop = readStr(propAddr);
      const value = readStr(valueAddr);
      n.styles[prop] = value;
      if (n.el) n.el.style.setProperty(prop, value);
    },
    dom_get_style(h, propAddr) {
      const n = node(h);
      if (!n) return makeStr("");
      const prop = readStr(propAddr);
      if (n.el) return makeStr(n.el.style.getPropertyValue(prop) || "");
      return makeStr(n.styles[prop] ?? "");
    },
    dom_get_computed_style(h, propAddr) {
      const n = node(h);
      if (!n) return makeStr("");
      const prop = readStr(propAddr);
      if (n.el && mode === "browser") {
        return makeStr(getComputedStyle(n.el).getPropertyValue(prop) || "");
      }
      return makeStr(n.styles[prop] ?? "");
    },
    dom_client_width(h) {
      const n = node(h);
      if (!n) return 0n;
      if (n.el) return BigInt(n.el.clientWidth || 0);
      return BigInt(n.width || 0);
    },
    dom_client_height(h) {
      const n = node(h);
      if (!n) return 0n;
      if (n.el) return BigInt(n.el.clientHeight || 0);
      return BigInt(n.height || 0);
    },
    dom_offset_width(h) {
      const n = node(h);
      if (!n) return 0n;
      if (n.el) return BigInt(n.el.offsetWidth || 0);
      return BigInt(n.width || 0);
    },
    dom_offset_height(h) {
      const n = node(h);
      if (!n) return 0n;
      if (n.el) return BigInt(n.el.offsetHeight || 0);
      return BigInt(n.height || 0);
    },
    dom_get_bounding_rect(h) {
      const n = node(h);
      if (!n) return makeStr("0,0,0,0");
      if (n.el && mode === "browser") {
        const r = n.el.getBoundingClientRect();
        return makeStr(`${r.x},${r.y},${r.width},${r.height}`);
      }
      return makeStr(`0,0,${n.width || 0},${n.height || 0}`);
    },
    dom_focus(h) {
      const n = node(h);
      if (n?.el) n.el.focus();
    },
    dom_blur(h) {
      const n = node(h);
      if (n?.el) n.el.blur();
    },
    dom_parent(h) {
      const n = node(h);
      return n ? BigInt(n.parent || 0) : 0n;
    },
    dom_child_count(h) {
      const n = node(h);
      return n ? BigInt(n.children.length) : 0n;
    },
    dom_child_at(h, index) {
      const n = node(h);
      if (!n) return 0n;
      const i = Number(index);
      if (i < 0 || i >= n.children.length) return 0n;
      return BigInt(n.children[i]);
    },
    dom_dataset_set(h, keyAddr, valueAddr) {
      const n = node(h);
      if (!n) return;
      const key = readStr(keyAddr);
      const value = readStr(valueAddr);
      n.attrs["data-" + key] = value;
      if (n.el) n.el.dataset[key] = value;
    },
    dom_dataset_get(h, keyAddr) {
      const n = node(h);
      if (!n) return makeStr("");
      const key = readStr(keyAddr);
      if (n.el) return makeStr(n.el.dataset[key] ?? "");
      return makeStr(n.attrs["data-" + key] ?? "");
    },
    dom_scroll_to(h, x, y) {
      const n = node(h);
      if (n?.el) n.el.scrollTo(Number(x), Number(y));
    },
    dom_add_listener(h, eventAddr, handlerAddr) {
      const n = node(h);
      if (!n) return;
      const event = readStr(eventAddr);
      const handler = readStr(handlerAddr);
      listeners.set(`${Number(h)}:${event}`, handler);
      ensureBrowserListener(n, h, event);
    },
    dom_remove_listener(h, eventAddr) {
      const event = readStr(eventAddr);
      listeners.delete(`${Number(h)}:${event}`);
    },
    dom_dispatch(h, eventAddr) {
      fire(h, readStr(eventAddr));
    },
    dom_last_event_type() {
      return makeStr(lastEventType);
    },
    dom_last_event_target() {
      return BigInt(lastEventTarget);
    },
  };

  return api;
}
