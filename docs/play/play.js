/* Loom docs: the playground.
 *
 * Every sample on a page becomes an editor with a Run button. Running one
 * compiles and runs it in the browser with the real Loom compiler and Treadle,
 * built for WebAssembly (crates/loom_play, packed into loom-wasm.js by
 * docs/play/build.py), so what you see is what `loom run` says.
 *
 * It all works from file:// (that's how `loom docs` opens the pages): the wasm
 * comes in a classic <script>, the worker is made from a Blob, and nothing is
 * fetched. The wasm loads the first time an editor is focused or a sample run.
 *
 * Programs run in a Web Worker with a small WASI shim, so the page stays
 * responsive, and a run that takes too long (or that you stop) ends with its
 * worker. Without JavaScript the pages still show every sample and its
 * checked output; this file only adds to them.
 */
(function () {
  "use strict";

  const SCRIPT = document.currentScript;
  const BASE = SCRIPT && SCRIPT.src ? SCRIPT.src.replace(/[^/]*$/, "") : "play/";
  const TIME_LIMIT_MS = 10000;
  const OUTPUT_LIMIT = 256 * 1024;
  const STORE = "loom.sample.";

  // ------------------------------------------------------------ highlighting
  // Mirrors highlight() in docs/common.py, so a page looks the same before
  // and after this script takes over.

  const KEYWORDS = new Set(("fun let var const type warp weft weave into through case pub if else match for in " +
    "while loop break continue return fail is not and or mend defer ref spool given loom thread yield pull " +
    "unsafe extern use as any self Self mut take once test where true false some none ok err").split(" "));
  const NUMBER = /0x[0-9A-Fa-f_?a-z]+|0b[01_?a-z]+|0o[0-7_]+|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][-+]?\d+)?[a-z]\w*|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][-+]?\d+)?/y;
  const IDENT = /[A-Za-z_]\w*/y;
  const CHAR = /'(?:\\.[^']*|[^'\\])'/y;
  const ATTR = /#\w+/y;

  function esc(s) {
    return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  }

  function at(re, code, i) {
    re.lastIndex = i;
    const m = re.exec(code);
    return m ? m[0] : null;
  }

  // Scans Loom source into [class, text] pieces. Inside a template's `{...}`
  // it stops after the matching `}`. Returns [pieces, next index].
  function tokens(code, i, inTemplate) {
    const out = [];
    let depth = 0;
    const n = code.length;
    while (i < n) {
      const c = code[i];
      let m;
      if (c === "/" && code.startsWith("//", i)) {
        let j = code.indexOf("\n", i);
        if (j < 0) j = n;
        out.push(["c", code.slice(i, j)]);
        i = j;
      } else if (c === "/" && code.startsWith("/*", i)) {
        let j = code.indexOf("*/", i + 2);
        j = j < 0 ? n : j + 2;
        out.push(["c", code.slice(i, j)]);
        i = j;
      } else if (c === '"') {
        const r = string(code, i);
        out.push(...r[0]);
        i = r[1];
      } else if (c === "'" && (m = at(CHAR, code, i))) {
        out.push(["s", m]);
        i += m.length;
      } else if (c >= "0" && c <= "9") {
        m = at(NUMBER, code, i);
        out.push(["n", m]);
        i += m.length;
      } else if (/[A-Za-z_]/.test(c)) {
        m = at(IDENT, code, i);
        const prev = i ? code[i - 1] : "";
        const next = code[i + m.length];
        if (KEYWORDS.has(m) && prev !== ".") out.push(["k", m]);
        else if (m[0] >= "A" && m[0] <= "Z" && /[a-z]/.test(m)) out.push(["t", m]);
        else if (next === "(") out.push(["f", m]);
        else out.push(["", m]);
        i += m.length;
      } else if (c === "#" && (m = at(ATTR, code, i))) {
        out.push(["a", m]);
        i += m.length;
      } else {
        if (inTemplate) {
          if (c === "{") depth++;
          else if (c === "}") {
            if (depth === 0) return [out, i];
            depth--;
          }
        }
        out.push(["", c]);
        i++;
      }
    }
    return [out, i];
  }

  function string(code, i) {
    const out = [];
    let buf = '"';
    i++;
    const n = code.length;
    while (i < n) {
      const c = code[i];
      if (c === "\\" && i + 1 < n) {
        buf += code.slice(i, i + 2);
        i += 2;
      } else if (c === '"') {
        buf += '"';
        i++;
        break;
      } else if (c === "{" && code.startsWith("{{", i)) {
        buf += "{{";
        i += 2;
      } else if (c === "{") {
        out.push(["s", buf]);
        out.push(["h", "{"]);
        buf = "";
        const r = tokens(code, i + 1, true);
        out.push(...r[0]);
        i = r[1];
        if (i < n && code[i] === "}") out.push(["h", "}"]);
        i++;
      } else {
        buf += c;
        i++;
      }
    }
    if (buf) out.push(["s", buf]);
    return [out, i];
  }

  function highlight(code) {
    return tokens(code, 0, false)[0]
      .map(([cls, text]) => (cls ? `<span class="${cls}">${esc(text)}</span>` : esc(text)))
      .join("");
  }

  // Compiler output: `error[CODE]:` lines, `-->` locations (links into the
  // editor when they name its file), and the gutter of a snippet.
  function highlightOutput(text, file) {
    return text.split("\n").map((ln) => {
      const e = esc(ln);
      if (/^(error|warning)(\[[A-Z]\d{4}\])?:/.test(ln) || /^unravel:/.test(ln)) {
        return `<span class="${ln.startsWith("warning") ? "warn" : "err"}">${e}</span>`;
      }
      const loc = /^(\s*-->\s*)(\S+?):(\d+):(\d+)\s*$/.exec(ln);
      if (loc) {
        const inner = `${esc(loc[2])}:${loc[3]}:${loc[4]}`;
        if (file && loc[2] === file) {
          return `${esc(loc[1])}<a class="loc" data-line="${loc[3]}" data-col="${loc[4]}" tabindex="0">${inner}</a>`;
        }
        return `<span class="loc">${e}</span>`;
      }
      const gut = /^(\s*\d*\s*\|)(.*)$/.exec(ln);
      if (gut) {
        const rest = gut[2];
        if (/^\s*[\^~-]+/.test(rest) || /^\s*\^/.test(rest)) {
          return `<span class="gut">${esc(gut[1])}</span><span class="caret">${esc(rest)}</span>`;
        }
        return `<span class="gut">${esc(gut[1])}</span>${esc(rest)}`;
      }
      if (/^\s*= (note|help)/.test(ln)) return `<span class="loc">${e}</span>`;
      return e;
    }).join("\n");
  }

  // ------------------------------------------------------------ the wasm

  let wasmPromise = null;

  function loadScript(src) {
    return new Promise((resolve, reject) => {
      const s = document.createElement("script");
      s.src = src;
      s.onload = resolve;
      s.onerror = () => reject(new Error(`couldn't load ${src}`));
      document.head.appendChild(s);
    });
  }

  // The module's bytes: base64, then gzip, undone.
  function wasmBytes() {
    if (!wasmPromise) {
      wasmPromise = (async () => {
        if (!window.LOOM_WASM) await loadScript(BASE + "loom-wasm.js");
        if (typeof DecompressionStream === "undefined") throw new Error("this browser can't unpack the compiler (no DecompressionStream)");
        const raw = atob(window.LOOM_WASM);
        const packed = new Uint8Array(raw.length);
        for (let i = 0; i < raw.length; i++) packed[i] = raw.charCodeAt(i);
        const stream = new Blob([packed]).stream().pipeThrough(new DecompressionStream("gzip"));
        const bytes = new Uint8Array(await new Response(stream).arrayBuffer());
        const module = await WebAssembly.compile(bytes);
        return { bytes, module };
      })();
      wasmPromise.catch(() => { wasmPromise = null; });
    }
    return wasmPromise;
  }

  // ------------------------------------------------------------ the worker
  // This function is the worker's whole source (it's stringified into a Blob):
  // a WASI preview 1 shim just big enough for `loom-play`.

  function workerMain() {
    let module = null;
    const queue = [];

    self.onmessage = async (e) => {
      const msg = e.data;
      if (msg.type === "module") {
        module = msg.module;
      } else if (msg.type === "bytes") {
        module = await WebAssembly.compile(msg.bytes);
      } else if (msg.type === "run") {
        queue.push(msg);
      }
      while (module && queue.length) await play(queue.shift());
    };

    class Exit {
      constructor(code) { this.code = code; }
    }

    const ESUCCESS = 0, EBADF = 8, ENOSYS = 52, ENOTCAPABLE = 76;

    async function play({ id, name, source, mode, limit }) {
      const enc = new TextEncoder();
      const stdin = enc.encode(source);
      let stdinAt = 0;
      const args = ["loom-play", name, mode].map((a) => enc.encode(a + "\0"));
      const decoders = { 1: new TextDecoder(), 2: new TextDecoder() };
      let pending = [];
      let pendingSize = 0;
      let lastFlush = performance.now();
      let written = 0;
      let truncated = false;
      let memory = null;
      const view = () => new DataView(memory.buffer);
      const bytes = () => new Uint8Array(memory.buffer);

      function flush() {
        if (pending.length) self.postMessage({ type: "out", id, chunks: pending });
        pending = [];
        pendingSize = 0;
        lastFlush = performance.now();
      }

      function emit(fd, data) {
        if (truncated) return;
        written += data.length;
        if (written > limit) {
          truncated = true;
          pending.push([2, "\n[the playground stopped showing output here: there was too much]\n"]);
          flush();
          return;
        }
        const text = decoders[fd].decode(data, { stream: true });
        if (!text) return;
        const last = pending[pending.length - 1];
        if (last && last[0] === fd) last[1] += text;
        else pending.push([fd, text]);
        pendingSize += text.length;
        if (pendingSize > 8192 || performance.now() - lastFlush > 40) flush();
      }

      function iovecs(ptr, len) {
        const v = view();
        const out = [];
        for (let i = 0; i < len; i++) out.push([v.getUint32(ptr + i * 8, true), v.getUint32(ptr + i * 8 + 4, true)]);
        return out;
      }

      const wasi = {
        args_sizes_get(argc, size) {
          const v = view();
          v.setUint32(argc, args.length, true);
          v.setUint32(size, args.reduce((n, a) => n + a.length, 0), true);
          return ESUCCESS;
        },
        args_get(argv, buf) {
          const v = view();
          args.forEach((a, i) => {
            v.setUint32(argv + i * 4, buf, true);
            bytes().set(a, buf);
            buf += a.length;
          });
          return ESUCCESS;
        },
        environ_sizes_get(count, size) {
          const v = view();
          v.setUint32(count, 0, true);
          v.setUint32(size, 0, true);
          return ESUCCESS;
        },
        environ_get() { return ESUCCESS; },
        random_get(ptr, len) {
          const b = bytes();
          for (let i = 0; i < len; i += 65536) crypto.getRandomValues(b.subarray(ptr + i, ptr + Math.min(len, i + 65536)));
          return ESUCCESS;
        },
        clock_res_get(id, out) {
          view().setBigUint64(out, 1000n, true);
          return ESUCCESS;
        },
        clock_time_get(id, precision, out) {
          const ms = id === 0 ? Date.now() : performance.now();
          view().setBigUint64(out, BigInt(Math.round(ms * 1e6)), true);
          return ESUCCESS;
        },
        fd_write(fd, iovs, len, nwritten) {
          if (fd !== 1 && fd !== 2) return EBADF;
          let n = 0;
          for (const [ptr, size] of iovecs(iovs, len)) {
            emit(fd, bytes().slice(ptr, ptr + size));
            n += size;
          }
          view().setUint32(nwritten, n, true);
          return ESUCCESS;
        },
        fd_read(fd, iovs, len, nread) {
          if (fd !== 0) return EBADF;
          let n = 0;
          for (const [ptr, size] of iovecs(iovs, len)) {
            const chunk = stdin.subarray(stdinAt, stdinAt + size);
            bytes().set(chunk, ptr);
            stdinAt += chunk.length;
            n += chunk.length;
            if (chunk.length < size) break;
          }
          view().setUint32(nread, n, true);
          return ESUCCESS;
        },
        fd_close(fd) { return fd <= 2 ? ESUCCESS : EBADF; },
        fd_fdstat_get(fd, out) {
          if (fd > 2) return EBADF;
          const v = view();
          v.setUint8(out, 2); // a character device
          v.setUint16(out + 2, 0, true);
          v.setBigUint64(out + 8, 0xffffffffn, true);
          v.setBigUint64(out + 16, 0xffffffffn, true);
          return ESUCCESS;
        },
        fd_filestat_get() { return EBADF; },
        fd_prestat_get() { return EBADF; },
        fd_prestat_dir_name() { return EBADF; },
        fd_readdir() { return EBADF; },
        path_open() { return ENOTCAPABLE; },
        path_create_directory() { return ENOTCAPABLE; },
        path_filestat_get() { return ENOTCAPABLE; },
        path_rename() { return ENOTCAPABLE; },
        path_unlink_file() { return ENOTCAPABLE; },
        sched_yield() { return ESUCCESS; },
        // Sleeps (the sandbox shouldn't ask for any) wait here, briefly.
        poll_oneoff(subs, events, n, nevents) {
          const v = view();
          let wait = 0;
          for (let i = 0; i < n; i++) {
            const s = subs + i * 48;
            if (v.getUint8(s + 8) === 0) {
              const ns = Number(v.getBigUint64(s + 24, true));
              const abs = v.getUint16(s + 40, true) & 1;
              wait = Math.max(wait, abs ? ns / 1e6 - performance.now() : ns / 1e6);
            }
            const ev = events + i * 32;
            v.setBigUint64(ev, v.getBigUint64(s, true), true);
            v.setUint16(ev + 8, 0, true);
            v.setUint8(ev + 10, v.getUint8(s + 8));
          }
          const until = performance.now() + Math.min(Math.max(wait, 0), 2000);
          while (performance.now() < until) { /* a worker may spin */ }
          v.setUint32(nevents, n, true);
          return ESUCCESS;
        },
        proc_exit(code) { throw new Exit(code); },
      };

      const imports = {};
      for (const imp of WebAssembly.Module.imports(module)) {
        if (imp.kind !== "function") continue;
        (imports[imp.module] = imports[imp.module] || {})[imp.name] = wasi[imp.name] || (() => ENOSYS);
      }
      let code;
      const t = performance.now();
      try {
        const instance = await WebAssembly.instantiate(module, imports);
        memory = instance.exports.memory;
        instance.exports._start();
        code = 0;
      } catch (e) {
        if (e instanceof Exit) {
          code = e.code;
        } else {
          pending.push([2, `internal error: the playground crashed (${e && e.message ? e.message : e})\n` +
            "  = note: this is a bug in Loom, not in your program\n"]);
          code = 70;
        }
      }
      for (const fd of [1, 2]) {
        const rest = decoders[fd].decode();
        if (rest) pending.push([fd, rest]);
      }
      flush();
      self.postMessage({ type: "done", id, code, ms: Math.round(performance.now() - t) });
    }
  }

  let worker = null;
  let workerReady = null;
  let current = null;
  let nextId = 1;

  function startWorker() {
    if (worker) return workerReady;
    const src = `(${workerMain.toString()})();`;
    worker = new Worker(URL.createObjectURL(new Blob([src], { type: "text/javascript" })));
    worker.onmessage = (e) => {
      const msg = e.data;
      if (!current || msg.id !== current.id) return;
      if (msg.type === "out") {
        for (const [fd, text] of msg.chunks) current.onChunk(fd, text);
      } else if (msg.type === "done") {
        finish({ code: msg.code, ms: msg.ms });
      }
    };
    worker.onerror = (e) => {
      e.preventDefault();
      if (current) {
        current.onChunk(2, `internal error: the playground's worker failed (${e.message || "unknown"})\n`);
        finish({ code: 70, ms: Math.round(performance.now() - current.started) });
      }
      killWorker();
    };
    const w = worker;
    workerReady = wasmBytes().then(({ bytes, module }) => {
      try {
        w.postMessage({ type: "module", module });
      } catch (_) {
        w.postMessage({ type: "bytes", bytes });
      }
    });
    workerReady.catch(() => killWorker());
    return workerReady;
  }

  function killWorker() {
    if (worker) worker.terminate();
    worker = null;
    workerReady = null;
  }

  function finish(result) {
    const run = current;
    if (!run) return;
    current = null;
    clearTimeout(run.timer);
    run.resolve(result);
  }

  // Stops the run going on, if any: its worker goes, and a fresh one starts.
  function stop(why) {
    if (!current) return;
    const ms = Math.round(performance.now() - current.started);
    killWorker();
    finish({ code: null, ms, stopped: why });
  }

  // Runs `source` as the file `name` (`run`, `check` or `test`); `onChunk(fd,
  // text)` gets the output as it comes. Resolves to {code, ms} or, for a run
  // that was stopped, {code: null, stopped: "you" | "time" | "other"}.
  function play(name, source, mode, onChunk) {
    stop("other");
    return new Promise((resolve) => {
      const run = { id: nextId++, onChunk, resolve, started: performance.now(), timer: null };
      current = run;
      startWorker().then(() => {
        if (current !== run) return;
        run.started = performance.now();
        run.timer = setTimeout(() => { if (current === run) stop("time"); }, TIME_LIMIT_MS);
        worker.postMessage({ type: "run", id: run.id, name, source, mode, limit: OUTPUT_LIMIT });
      }, (err) => {
        if (current !== run) return;
        onChunk(2, `the playground couldn't start: ${err && err.message ? err.message : err}\n`);
        finish({ code: null, ms: 0, stopped: "broken" });
      });
    });
  }

  // ------------------------------------------------------------ the editor

  function el(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  }

  const mac = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);

  class Editor {
    constructor(host, text, onRun) {
      this.host = host;
      this.onRun = onRun;
      host.className = "editor";
      host.textContent = "";
      this.lines = el("div", "ed-lines");
      this.lines.setAttribute("aria-hidden", "true");
      this.pre = el("pre", "ed-hl");
      this.pre.setAttribute("aria-hidden", "true");
      this.code = el("code");
      this.pre.appendChild(this.code);
      this.ta = el("textarea");
      this.ta.spellcheck = false;
      this.ta.wrap = "off";
      for (const [k, v] of [["autocapitalize", "off"], ["autocomplete", "off"], ["autocorrect", "off"],
        ["aria-label", `Loom code. Edit it, then run it with ${mac ? "Cmd" : "Ctrl"}+Enter. Escape leaves the editor.`]]) {
        this.ta.setAttribute(k, v);
      }
      this.bands = [];
      host.append(this.lines, this.pre, this.ta);
      this.ta.value = text;
      this.lineCount = -1;
      this.render();
      this.ta.addEventListener("input", () => { this.clearMarks(); this.render(); });
      this.ta.addEventListener("scroll", () => { this.pre.scrollLeft = this.ta.scrollLeft; });
      this.ta.addEventListener("keydown", (e) => this.key(e));
    }

    get value() { return this.ta.value; }

    set value(v) {
      this.ta.value = v;
      this.clearMarks();
      this.render();
    }

    render() {
      const v = this.ta.value;
      this.code.innerHTML = highlight(v) + "\n";
      const n = v.split("\n").length;
      if (n !== this.lineCount) {
        this.lineCount = n;
        let s = "";
        for (let i = 1; i <= n; i++) s += (i > 1 ? "\n" : "") + i;
        this.lines.textContent = s;
        this.ta.rows = n;
      }
      this.pre.scrollLeft = this.ta.scrollLeft;
    }

    // Inserts text where the selection is, keeping the browser's undo.
    insert(text) {
      this.ta.focus();
      if (!document.execCommand || !document.execCommand("insertText", false, text)) {
        this.ta.setRangeText(text, this.ta.selectionStart, this.ta.selectionEnd, "end");
        this.ta.dispatchEvent(new Event("input"));
      }
    }

    key(e) {
      const ta = this.ta;
      const v = ta.value;
      const s = ta.selectionStart;
      const end = ta.selectionEnd;
      if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        this.onRun();
        return;
      }
      if (e.key === "Escape") {
        ta.blur();
        const run = this.host.closest(".sample")?.querySelector(".btn.run");
        if (run) run.focus();
        return;
      }
      if (e.key === "Tab") {
        e.preventDefault();
        const lineStart = v.lastIndexOf("\n", s - 1) + 1;
        if (s === end && !e.shiftKey) {
          const col = s - lineStart;
          this.insert(" ".repeat(4 - (col % 4)));
          return;
        }
        // Indent or dedent every line the selection touches.
        const blockEnd = v.indexOf("\n", end - (end > s && v[end - 1] === "\n" ? 1 : 0));
        const stop = blockEnd < 0 ? v.length : blockEnd;
        const block = v.slice(lineStart, stop);
        const changed = block.split("\n").map((ln) => (e.shiftKey ? ln.replace(/^ {1,4}/, "") : (ln ? "    " + ln : ln))).join("\n");
        ta.setSelectionRange(lineStart, stop);
        this.insert(changed);
        ta.setSelectionRange(lineStart, lineStart + changed.length);
        return;
      }
      if (e.key === "Enter" && !e.shiftKey && !e.altKey) {
        e.preventDefault();
        const lineStart = v.lastIndexOf("\n", s - 1) + 1;
        const indent = /^ */.exec(v.slice(lineStart, s))[0];
        const before = v.slice(lineStart, s).trimEnd();
        const opens = /[{([]$/.test(before);
        const after = v[end];
        const closes = opens && after && "})]".includes(after);
        if (closes) {
          this.insert("\n" + indent + "    " + "\n" + indent);
          const at = s + 1 + indent.length + 4;
          ta.setSelectionRange(at, at);
        } else {
          this.insert("\n" + indent + (opens ? "    " : ""));
        }
        return;
      }
      if ((e.key === "}" || e.key === ")" || e.key === "]") && s === end) {
        const lineStart = v.lastIndexOf("\n", s - 1) + 1;
        const head = v.slice(lineStart, s);
        if (/^ {4,}$/.test(head)) {
          e.preventDefault();
          ta.setSelectionRange(s - 4, s);
          this.insert(e.key);
        }
      }
    }

    clearMarks() {
      for (const b of this.bands) b.remove();
      this.bands = [];
      for (const n of this.lines.querySelectorAll(".bad")) n.classList.remove("bad");
    }

    // Marks lines the compiler or an unravel pointed at.
    mark(lines) {
      this.clearMarks();
      const cs = getComputedStyle(this.ta);
      const lh = parseFloat(cs.lineHeight) || 20;
      const top = parseFloat(cs.paddingTop) || 0;
      const nums = this.lines.textContent.split("\n");
      const marked = new Set(lines);
      this.lines.innerHTML = nums.map((x, i) => (marked.has(i + 1) ? `<span class="bad">${x}</span>` : x)).join("\n");
      for (const line of marked) {
        if (line < 1 || line > nums.length) continue;
        const band = el("div", "ed-band");
        band.style.top = `${top + (line - 1) * lh}px`;
        this.host.appendChild(band);
        this.bands.push(band);
      }
    }

    jump(line, col) {
      const v = this.ta.value;
      let i = 0;
      for (let l = 1; l < line && i >= 0; l++) i = v.indexOf("\n", i) + 1;
      if (i < 0) return;
      const at = Math.min(i + Math.max(col - 1, 0), v.length);
      this.ta.focus();
      this.ta.setSelectionRange(at, at);
      const lh = parseFloat(getComputedStyle(this.ta).lineHeight) || 20;
      const rect = this.ta.getBoundingClientRect();
      const y = rect.top + window.scrollY + (line - 1) * lh;
      if (y < window.scrollY + 80 || y > window.scrollY + window.innerHeight - 80) {
        window.scrollTo({ top: y - window.innerHeight / 3, behavior: "smooth" });
      }
    }
  }

  // ------------------------------------------------------------ samples

  function store(key, value) {
    try {
      if (value == null) localStorage.removeItem(key);
      else localStorage.setItem(key, value);
    } catch (_) { /* storage may be off: edits just don't stick */ }
  }

  function stored(key) {
    try { return localStorage.getItem(key); } catch (_) { return null; }
  }

  const VERB = { run: "Run", check: "Check", test: "Test" };

  function playIcon() {
    return '<svg class="icon" viewBox="0 0 12 12" aria-hidden="true"><path d="M2.5 1.5v9l8-4.5z" fill="currentColor"/></svg>';
  }
  function stopIcon() {
    return '<svg class="icon" viewBox="0 0 12 12" aria-hidden="true"><rect x="2" y="2" width="8" height="8" rx="1.5" fill="currentColor"/></svg>';
  }

  function enhance(sample) {
    const name = sample.dataset.sample;
    const mode = sample.dataset.mode || "run";
    const file = sample.dataset.file || `${name}.loom`;
    const pre = sample.querySelector("pre.code");
    const head = sample.querySelector(".sample-head");
    if (!pre || !head) return;
    const original = pre.textContent.replace(/\n$/, "");
    const key = STORE + (sample.dataset.store || name);
    const saved = stored(key);
    const host = el("div");
    pre.replaceWith(host);

    const runBtn = el("button", "btn run");
    runBtn.type = "button";
    const reset = el("button", "btn ghost", "Reset");
    reset.type = "button";
    reset.title = "Put the sample back as it was";
    const hint = el("span", "sample-hint");
    hint.innerHTML = `<kbd>${mac ? "⌘" : "Ctrl"}</kbd>+<kbd>Enter</kbd>`;
    head.append(hint, reset, runBtn);

    let running = false;
    // The playground page switches a sample's mode (dataset.mode) as it loads others.
    const modeNow = () => sample.dataset.mode || mode;
    const setRunning = (on) => {
      running = on;
      const verb = VERB[modeNow()] || "Run";
      runBtn.classList.toggle("running", on);
      runBtn.innerHTML = on ? `${stopIcon()} Stop` : `${playIcon()} ${verb}`;
      runBtn.setAttribute("aria-label", on ? "Stop the program" : `${verb} this sample`);
    };
    setRunning(false);
    sample.addEventListener("loom:mode", () => { if (!running) setRunning(false); });

    const editor = new Editor(host, saved != null ? saved : original, () => go());
    const edited = () => {
      const is = editor.value !== original;
      sample.classList.toggle("edited", is);
      reset.hidden = !is;
      return is;
    };
    edited();
    let saveTimer = null;
    editor.ta.addEventListener("input", () => {
      edited();
      clearTimeout(saveTimer);
      saveTimer = setTimeout(() => store(key, editor.value !== original ? editor.value : null), 250);
    });
    editor.ta.addEventListener("focus", () => { startWorker().catch(() => {}); }, { once: true });
    reset.addEventListener("click", () => {
      editor.value = original;
      store(key, null);
      edited();
      editor.ta.focus();
    });

    async function go() {
      if (running) {
        stop("you");
        return;
      }
      setRunning(true);
      for (const old of sample.querySelectorAll(".sample-out")) old.remove();
      const box = el("div", "sample-out live");
      const outHead = el("div", "out-head");
      const mode = modeNow();
      const title = el("span", null, mode === "test" ? "Test report" : "Output");
      const meta = el("span", "meta", "compiling…");
      outHead.append(title, meta);
      const outPre = el("pre");
      const outCode = el("code");
      outPre.appendChild(outCode);
      outCode.innerHTML = '<span class="cursor"></span>';
      box.append(outHead, outPre);
      sample.appendChild(box);
      box.setAttribute("aria-live", "polite");

      const chunks = [];
      let queued = false;
      let finished = false;
      const paint = (done) => {
        queued = false;
        if (finished && !done) return;
        let html = "";
        for (const [fd, text] of chunks) {
          html += fd === 2 ? `<span class="stderr">${highlightOutput(text, file)}</span>` : esc(text);
        }
        outCode.innerHTML = html + (done ? "" : '<span class="cursor"></span>');
      };
      const onChunk = (fd, text) => {
        const last = chunks[chunks.length - 1];
        if (last && last[0] === fd) last[1] += text;
        else chunks.push([fd, text]);
        if (meta.textContent === "compiling…") meta.textContent = "running…";
        if (!queued) {
          queued = true;
          requestAnimationFrame(() => paint(false));
        }
      };

      const result = await play(file, editor.value, mode, onChunk);
      finished = true;
      setRunning(false);
      paint(true);
      const stderr = chunks.filter((c) => c[0] === 2).map((c) => c[1]).join("");
      const stdout = chunks.filter((c) => c[0] === 1).map((c) => c[1]).join("");
      if (result.stopped) {
        const why = { you: "stopped", time: `stopped after ${TIME_LIMIT_MS / 1000} s`, other: "stopped: another sample started", broken: "couldn't run" }[result.stopped];
        meta.textContent = why;
        if (result.stopped === "time") {
          outCode.insertAdjacentHTML("beforeend", `${chunks.length ? "\n" : ""}<span class="note-line">(it ran for ${TIME_LIMIT_MS / 1000} seconds without finishing: is there a loop that never ends?)</span>`);
        }
        box.classList.add("failed");
        return;
      }
      const code = result.code;
      if (code === 2 && !stdout) title.textContent = "The compiler says";
      else if (mode === "check" && code === 0) title.textContent = "The compiler says";
      meta.textContent = `exit ${code} · ${result.ms} ms`;
      box.classList.toggle("failed", code !== 0);
      if (!chunks.length) outCode.innerHTML = '<span class="note-line">(no output)</span>';
      const bad = [];
      const re = new RegExp(`-->\\s*${file.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}:(\\d+):\\d+`, "g");
      let m;
      while ((m = re.exec(stderr + stdout))) bad.push(Number(m[1]));
      editor.mark(bad);
      for (const a of outCode.querySelectorAll("a.loc")) {
        const jump = () => editor.jump(Number(a.dataset.line), Number(a.dataset.col));
        a.addEventListener("click", jump);
        a.addEventListener("keydown", (e) => { if (e.key === "Enter") jump(); });
      }
    }

    runBtn.addEventListener("click", go);
  }

  function init() {
    for (const s of document.querySelectorAll(".sample[data-runnable]")) {
      try {
        enhance(s);
      } catch (e) {
        console.error("playground:", e);
      }
    }
  }

  window.LoomPlay = { highlight, highlightOutput, play, stop, enhance };
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init);
  else init();
})();
