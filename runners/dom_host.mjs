// Shared DOM host for machino WASM modules.
// mode: "browser" uses the real document; "virtual" uses an in-memory tree.

export function createDomHost({ readStr, makeStr, mode = "virtual" }) {
  const nodes = new Map();
  let next = 2;
  nodes.set(1, {
    tag: "#document",
    text: "",
    attrs: {},
    styles: {},
    children: [],
    parent: 0,
    el: mode === "browser" ? document.documentElement : null,
  });

  function htmlOf(h) {
    const n = nodes.get(Number(h));
    if (!n) return "";
    if (n.tag === "#document") {
      return n.children.map(htmlOf).join("");
    }
    let s = `<${n.tag}`;
    for (const [k, v] of Object.entries(n.attrs)) s += ` ${k}="${v}"`;
    s += `>${n.text}`;
    for (const c of n.children) s += htmlOf(c);
    s += `</${n.tag}>`;
    return s;
  }

  return {
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
        el,
      });
      return h;
    },
    dom_get_element_by_id(idAddr) {
      const id = readStr(idAddr);
      if (mode === "browser") {
        const el = document.getElementById(id);
        if (!el) return 0n;
        for (const [h, n] of nodes) {
          if (n.el === el) return BigInt(h);
        }
        const h = BigInt(next++);
        nodes.set(Number(h), {
          tag: el.tagName.toLowerCase(),
          text: el.textContent ?? "",
          attrs: { id },
          styles: {},
          children: [],
          parent: 0,
          el,
        });
        return h;
      }
      for (const [h, n] of nodes) {
        if (n.attrs.id === id) return BigInt(h);
      }
      return 0n;
    },
    dom_query_selector(selAddr) {
      const sel = readStr(selAddr);
      if (mode === "browser") {
        const el = document.querySelector(sel);
        if (!el) return 0n;
        for (const [h, n] of nodes) {
          if (n.el === el) return BigInt(h);
        }
        const h = BigInt(next++);
        nodes.set(Number(h), {
          tag: el.tagName.toLowerCase(),
          text: el.textContent ?? "",
          attrs: {},
          styles: {},
          children: [],
          parent: 0,
          el,
        });
        return h;
      }
      if (sel.startsWith("#")) {
        const id = sel.slice(1);
        for (const [h, n] of nodes) {
          if (n.attrs.id === id) return BigInt(h);
        }
        return 0n;
      }
      for (const [h, n] of nodes) {
        if (n.tag === sel) return BigInt(h);
      }
      return 0n;
    },
    dom_set_text(h, textAddr) {
      const n = nodes.get(Number(h));
      if (!n) return;
      const text = readStr(textAddr);
      n.text = text;
      n.children = [];
      if (n.el) n.el.textContent = text;
    },
    dom_get_text(h) {
      const n = nodes.get(Number(h));
      if (!n) return makeStr("");
      if (n.el) return makeStr(n.el.textContent ?? "");
      return makeStr(n.text);
    },
    dom_set_html(h, htmlAddr) {
      const n = nodes.get(Number(h));
      if (!n) return;
      const html = readStr(htmlAddr);
      n.text = html;
      n.children = [];
      if (n.el) n.el.innerHTML = html;
    },
    dom_get_html(h) {
      const n = nodes.get(Number(h));
      if (!n) return makeStr("");
      if (n.el) return makeStr(n.el.innerHTML);
      return makeStr(htmlOf(h));
    },
    dom_set_attr(h, nameAddr, valueAddr) {
      const n = nodes.get(Number(h));
      if (!n) return;
      const name = readStr(nameAddr);
      const value = readStr(valueAddr);
      n.attrs[name] = value;
      if (n.el) n.el.setAttribute(name, value);
    },
    dom_get_attr(h, nameAddr) {
      const n = nodes.get(Number(h));
      if (!n) return makeStr("");
      const name = readStr(nameAddr);
      if (n.el) return makeStr(n.el.getAttribute(name) ?? "");
      return makeStr(n.attrs[name] ?? "");
    },
    dom_append_child(parentH, childH) {
      const p = nodes.get(Number(parentH));
      const c = nodes.get(Number(childH));
      if (!p || !c) return;
      if (c.parent) {
        const op = nodes.get(c.parent);
        if (op) op.children = op.children.filter((x) => x !== Number(childH));
      }
      c.parent = Number(parentH);
      if (!p.children.includes(Number(childH))) p.children.push(Number(childH));
      if (p.el && c.el) p.el.appendChild(c.el);
    },
    dom_remove_child(parentH, childH) {
      const p = nodes.get(Number(parentH));
      const c = nodes.get(Number(childH));
      if (!p || !c) return;
      p.children = p.children.filter((x) => x !== Number(childH));
      c.parent = 0;
      if (p.el && c.el && c.el.parentNode === p.el) p.el.removeChild(c.el);
    },
    dom_add_class(h, clsAddr) {
      const n = nodes.get(Number(h));
      if (!n) return;
      const cls = readStr(clsAddr);
      const cur = n.attrs.class ?? "";
      const parts = cur.split(/\s+/).filter(Boolean);
      if (!parts.includes(cls)) parts.push(cls);
      n.attrs.class = parts.join(" ");
      if (n.el) n.el.classList.add(cls);
    },
    dom_set_style(h, propAddr, valueAddr) {
      const n = nodes.get(Number(h));
      if (!n) return;
      const prop = readStr(propAddr);
      const value = readStr(valueAddr);
      n.styles[prop] = value;
      if (n.el) n.el.style.setProperty(prop, value);
    },
    dom_get_style(h, propAddr) {
      const n = nodes.get(Number(h));
      if (!n) return makeStr("");
      const prop = readStr(propAddr);
      if (n.el) return makeStr(n.el.style.getPropertyValue(prop) || "");
      return makeStr(n.styles[prop] ?? "");
    },
  };
}
