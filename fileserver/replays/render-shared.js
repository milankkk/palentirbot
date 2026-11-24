(function () {
  const DB_NAME = "osu_replays_drop_v1";
  const DB_STORE = "pending";
  const LS_KEY = "osu_render_config_v1";
  const COOKIE_KEY = "osu_render_prefs";
  const THEME_LS_KEY = "osu_site_theme_v1";
  const MAX_OSR_BYTES = 5 * 1024 * 1024;
  const PENDING_TTL_MS = 15 * 60 * 1000;

  const THEMES = [
    {
      id: "nord-dark",
      name: "Nord Dark",
      tagline: "Arctic dark default",
      mode: "dark",
      requiresLogin: false,
      themeColor: "#2e3440",
      preview: { bg: "#2e3440", surface: "#3b4252", text: "#eceff4", accent: "#88c0d0", accent2: "#81a1c1" },
    },
    {
      id: "nord-light",
      name: "Nord Light",
      tagline: "Snow Storm daylight",
      mode: "light",
      requiresLogin: false,
      themeColor: "#eceff4",
      preview: { bg: "#eceff4", surface: "#ffffff", text: "#2e3440", accent: "#5e81ac", accent2: "#88c0d0" },
    },
    {
      id: "midnight",
      name: "Classic Midnight",
      tagline: "Original obsidian & cyan",
      mode: "dark",
      requiresLogin: true,
      themeColor: "#0a0e17",
      preview: { bg: "#0a0e17", surface: "#121725", text: "#f3f6fb", accent: "#96dfe3", accent2: "#b066ff" },
    },
    {
      id: "osu-pink",
      name: "osu! Lazer",
      tagline: "Signature neon pink & plum",
      mode: "dark",
      requiresLogin: true,
      themeColor: "#150e19",
      preview: { bg: "#150e19", surface: "#211629", text: "#fdf2f8", accent: "#ff66aa", accent2: "#c084fc" },
    },
    {
      id: "catppuccin-mocha",
      name: "Catppuccin Mocha",
      tagline: "Cozy pastel dark",
      mode: "dark",
      requiresLogin: true,
      themeColor: "#1e1e2e",
      preview: { bg: "#1e1e2e", surface: "#313244", text: "#cdd6f4", accent: "#cba6f7", accent2: "#89dceb" },
    },
    {
      id: "sakura-light",
      name: "Sakura Blossom",
      tagline: "Cherry-blossom daylight",
      mode: "light",
      requiresLogin: true,
      themeColor: "#fdf3f7",
      preview: { bg: "#fdf3f7", surface: "#ffffff", text: "#341a28", accent: "#d63384", accent2: "#9b51e0" },
    },
    {
      id: "oled-pitch",
      name: "OLED Pitch",
      tagline: "Pure black AMOLED neon",
      mode: "dark",
      requiresLogin: true,
      themeColor: "#000000",
      preview: { bg: "#000000", surface: "#0e121a", text: "#f8fafc", accent: "#38f2ff", accent2: "#ff5ea8" },
    },
  ];
  const THEME_MAP = Object.create(null);
  THEMES.forEach((t) => { THEME_MAP[t.id] = t; });
  const DEFAULT_THEME_ID = "nord-dark";

  let authState = { loaded: false, logged_in: false, user: null, ok: false };
  let activeThemeId = DEFAULT_THEME_ID;
  let userPickedThemeThisSession = false;

  function findTheme(id) {
    return id && THEME_MAP[id] ? THEME_MAP[id] : null;
  }

  function isThemeUnlocked(id) {
    const t = findTheme(id);
    if (!t) return false;
    return !t.requiresLogin || Boolean(authState.logged_in);
  }

  function readStoredThemeId() {
    try {
      const saved = localStorage.getItem(THEME_LS_KEY);
      if (findTheme(saved)) return saved;
    } catch (_) {}
    const fromDom = document.documentElement && document.documentElement.dataset && document.documentElement.dataset.theme;
    if (findTheme(fromDom)) return fromDom;
    return DEFAULT_THEME_ID;
  }

  function updateThemeColorMeta(theme) {
    try {
      let meta = document.querySelector('meta[name="theme-color"]');
      if (!meta) {
        meta = document.createElement("meta");
        meta.name = "theme-color";
        document.head.appendChild(meta);
      }
      meta.content = theme.themeColor;
    } catch (_) {}
  }

  function applyTheme(id, opts) {
    const options = opts || {};
    const target = findTheme(id) || THEME_MAP[DEFAULT_THEME_ID];
    if (options.enforceAuth && target.requiresLogin && !authState.logged_in) {
      if (options.toastOnLock !== false) {
        showGlobalToast("Log in with osu! to unlock the " + target.name + " theme.", true);
      }
      return false;
    }

    activeThemeId = target.id;
    document.documentElement.dataset.theme = target.id;
    updateThemeColorMeta(target);

    if (options.persist !== false) {
      try { localStorage.setItem(THEME_LS_KEY, target.id); } catch (_) {}
    }
    if (options.userInitiated) {
      userPickedThemeThisSession = true;
    }
    if (options.syncServer && authState.logged_in) {
      fetch("/api/auth/theme", {
        method: "POST",
        credentials: "same-origin",
        cache: "no-store",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ theme: target.id }),
      })
        .then((r) => r.json())
        .then((d) => {
          if (d && d.ok && authState.user) {
            authState.user.theme = target.id;
          }
        })
        .catch(() => {});
    }

    renderThemeSwitcher();
    window.dispatchEvent(
      new CustomEvent("osu-theme-change", {
        detail: { theme: target, themes: THEMES, logged_in: Boolean(authState.logged_in) },
      })
    );
    return true;
  }

  function syncThemeWithAuth() {
    if (authState.logged_in && authState.user) {
      const serverTheme = findTheme(authState.user.theme);
      if (serverTheme && !userPickedThemeThisSession && serverTheme.id !== activeThemeId) {
        applyTheme(serverTheme.id, { persist: true, syncServer: false });
        return;
      }
      renderThemeSwitcher();
      window.dispatchEvent(
        new CustomEvent("osu-theme-change", {
          detail: { theme: THEME_MAP[activeThemeId] || THEME_MAP[DEFAULT_THEME_ID], themes: THEMES, logged_in: true },
        })
      );
      return;
    }

    // Only enforce guest downgrade when auth state was cleanly confirmed as logged out.
    if (authState.loaded && authState.ok && !authState.logged_in) {
      const cur = findTheme(activeThemeId) || THEME_MAP[DEFAULT_THEME_ID];
      if (cur.requiresLogin) {
        const fallback = cur.mode === "light" ? "nord-light" : "nord-dark";
        applyTheme(fallback, { persist: true, syncServer: false });
        return;
      }
    }
    renderThemeSwitcher();
  }

  // Sync initial activeThemeId immediately on script parse.
  activeThemeId = readStoredThemeId();
  document.documentElement.dataset.theme = activeThemeId;
  updateThemeColorMeta(THEME_MAP[activeThemeId] || THEME_MAP[DEFAULT_THEME_ID]);

  function openDropDb() {
    return new Promise((resolve, reject) => {
      if (!window.indexedDB) {
        reject(new Error("IndexedDB unavailable"));
        return;
      }
      const req = indexedDB.open(DB_NAME, 1);
      req.onupgradeneeded = () => {
        const db = req.result;
        if (!db.objectStoreNames.contains(DB_STORE)) {
          db.createObjectStore(DB_STORE);
        }
      };
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error || new Error("IndexedDB open failed"));
    });
  }

  async function savePendingReplay(file) {
    if (!file) throw new Error("No file selected.");
    if (!/\.osr$/i.test(file.name || "")) {
      throw new Error("Only .osr replay files are supported.");
    }
    if (file.size <= 0) throw new Error("Replay file is empty.");
    if (file.size > MAX_OSR_BYTES) {
      throw new Error("Replay file exceeds the 5 MB limit.");
    }
    const buffer = await file.arrayBuffer();
    const record = {
      name: file.name,
      size: file.size,
      buffer,
      savedAt: Date.now(),
    };
    try {
      const db = await openDropDb();
      await new Promise((resolve, reject) => {
        const tx = db.transaction(DB_STORE, "readwrite");
        tx.objectStore(DB_STORE).put(record, "latest");
        tx.oncomplete = () => resolve();
        tx.onerror = () => reject(tx.error);
      });
      db.close();
      return record;
    } catch (_) {
      const b64 = arrayBufferToBase64(buffer);
      sessionStorage.setItem(
        "osu_pending_osr_v1",
        JSON.stringify({ name: file.name, size: file.size, b64, savedAt: Date.now() })
      );
      return record;
    }
  }

  async function loadPendingReplay() {
    try {
      const db = await openDropDb();
      const rec = await new Promise((resolve, reject) => {
        const tx = db.transaction(DB_STORE, "readonly");
        const req = tx.objectStore(DB_STORE).get("latest");
        req.onsuccess = () => resolve(req.result || null);
        req.onerror = () => reject(req.error);
      });
      db.close();
      if (rec && rec.buffer && Date.now() - (rec.savedAt || 0) < PENDING_TTL_MS) {
        return rec;
      }
    } catch (_) {}

    try {
      const raw = sessionStorage.getItem("osu_pending_osr_v1");
      if (raw) {
        const parsed = JSON.parse(raw);
        if (parsed && parsed.b64 && Date.now() - (parsed.savedAt || 0) < PENDING_TTL_MS) {
          const bin = atob(parsed.b64);
          const arr = new Uint8Array(bin.length);
          for (let i = 0; i < bin.length; i++) arr[i] = bin.charCodeAt(i);
          return { name: parsed.name, size: parsed.size, buffer: arr.buffer, savedAt: parsed.savedAt };
        }
      }
    } catch (_) {}
    return null;
  }

  async function clearPendingReplay() {
    try { sessionStorage.removeItem("osu_pending_osr_v1"); } catch (_) {}
    try {
      const db = await openDropDb();
      await new Promise((resolve) => {
        const tx = db.transaction(DB_STORE, "readwrite");
        tx.objectStore(DB_STORE).delete("latest");
        tx.oncomplete = () => resolve();
        tx.onerror = () => resolve();
      });
      db.close();
    } catch (_) {}
  }

  function arrayBufferToBase64(buffer) {
    const bytes = new Uint8Array(buffer);
    const chunkSize = 0x8000;
    const chunks = [];
    for (let i = 0; i < bytes.length; i += chunkSize) {
      chunks.push(String.fromCharCode.apply(null, bytes.subarray(i, i + chunkSize)));
    }
    return btoa(chunks.join(""));
  }

  function readCookieFlags() {
    try {
      const pair = document.cookie
        .split("; ")
        .find((row) => row.startsWith(COOKIE_KEY + "="));
      if (!pair) return null;
      const val = decodeURIComponent(pair.split("=").slice(1).join("="));
      const params = new URLSearchParams(val);
      return {
        saveConfig: params.get("save") !== "0",
        skipSettings: params.get("skip") === "1",
      };
    } catch (_) {
      return null;
    }
  }

  function writeCookieFlags(saveConfig, skipSettings) {
    try {
      const val = encodeURIComponent(
        "save=" + (saveConfig ? "1" : "0") + "&skip=" + (skipSettings ? "1" : "0")
      );
      document.cookie = COOKIE_KEY + "=" + val + "; Path=/; Max-Age=31536000; SameSite=Lax";
    } catch (_) {}
  }

  function loadStoredConfig() {
    const cookieFlags = readCookieFlags();
    let stored = null;
    try {
      const raw = localStorage.getItem(LS_KEY);
      if (raw) stored = JSON.parse(raw);
    } catch (_) {}

    const base = {
      saveConfig: cookieFlags ? cookieFlags.saveConfig : true,
      skipSettings: cookieFlags ? cookieFlags.skipSettings : false,
      hasSavedSettings: false,
      userChoseResolution: false,
      settings: null,
      options: {
        pitch: "",
        offset_ms: "",
        skip_intro: false,
        force_rerender: false,
      },
      rawSettingsJson: "",
    };

    if (stored && typeof stored === "object") {
      if (typeof stored.saveConfig === "boolean") base.saveConfig = stored.saveConfig;
      if (typeof stored.skipSettings === "boolean") base.skipSettings = stored.skipSettings;
      if (typeof stored.userChoseResolution === "boolean") {
        base.userChoseResolution = stored.userChoseResolution;
      }
      if (stored.settings && typeof stored.settings === "object") {
        const rawRes = stored.settings.resolution;
        const res = (rawRes === "1080p" || rawRes === "900p") ? rawRes : "720p";
        base.settings = { ...stored.settings, resolution: res };
        base.hasSavedSettings = true;
      }
      if (stored.options && typeof stored.options === "object") {
        base.options = { ...base.options, ...stored.options };
      }
      if (typeof stored.rawSettingsJson === "string") {
        base.rawSettingsJson = stored.rawSettingsJson;
      }
    }
    return base;
  }

  function saveStoredConfig(cfg) {
    const saveFlag = Boolean(cfg.saveConfig);
    const skipFlag = saveFlag ? Boolean(cfg.skipSettings) : false;
    writeCookieFlags(saveFlag, skipFlag);
    try {
      if (saveFlag) {
        const rawRes = cfg.settings && cfg.settings.resolution;
        const res = (rawRes === "1080p" || rawRes === "900p") ? rawRes : "720p";
        const payload = {
          saveConfig: true,
          skipSettings: skipFlag,
          userChoseResolution: Boolean(cfg.userChoseResolution),
          settings: cfg.settings ? { ...cfg.settings, resolution: res } : null,
          options: {
            pitch: cfg.options && cfg.options.pitch != null ? String(cfg.options.pitch) : "",
            offset_ms: cfg.options && cfg.options.offset_ms != null ? String(cfg.options.offset_ms) : "",
            skip_intro: Boolean(cfg.options && cfg.options.skip_intro),
            force_rerender: Boolean(cfg.options && cfg.options.force_rerender),
          },
          rawSettingsJson: typeof cfg.rawSettingsJson === "string" ? cfg.rawSettingsJson : "",
          updatedAt: Date.now(),
        };
        localStorage.setItem(LS_KEY, JSON.stringify(payload));
      } else {
        localStorage.setItem(
          LS_KEY,
          JSON.stringify({ saveConfig: false, skipSettings: false, updatedAt: Date.now() })
        );
      }
    } catch (_) {}
  }

  function clearStoredConfig() {
    try { localStorage.removeItem(LS_KEY); } catch (_) {}
    try {
      document.cookie = COOKIE_KEY + "=; Path=/; Max-Age=0; SameSite=Lax";
    } catch (_) {}
  }

  function showGlobalToast(msg, isError) {
    let el = document.getElementById("global-drop-toast");
    if (!el) {
      el = document.createElement("div");
      el.id = "global-drop-toast";
      el.style.cssText =
        "position:fixed;bottom:22px;left:50%;transform:translateX(-50%);padding:10px 18px;" +
        "border-radius:10px;font-size:0.86rem;font-weight:600;z-index:99999;pointer-events:none;" +
        "box-shadow:var(--shadow-menu, 0 12px 32px rgba(0,0,0,0.45));transition:opacity 0.18s;font-family:inherit;";
      document.body.appendChild(el);
    }
    el.style.background = isError
      ? "var(--danger-bg, rgba(48, 12, 22, 0.96))"
      : "var(--menu-bg, rgba(46, 52, 64, 0.96))";
    el.style.border = isError
      ? "1px solid var(--danger-border, rgba(251, 113, 133, 0.6))"
      : "1px solid var(--accent-border, rgba(136, 192, 208, 0.5))";
    el.style.color = isError
      ? "var(--danger-text, #fda4af)"
      : "var(--text, #eceff4)";
    el.textContent = msg;
    el.style.opacity = "1";
    clearTimeout(el._t);
    el._t = setTimeout(() => { el.style.opacity = "0"; }, 2600);
  }

  function initGlobalDropRedirect() {
    if (location.pathname === "/render" || location.pathname === "/render/") return;

    const overlay = document.createElement("div");
    overlay.id = "global-osr-drop-overlay";
    overlay.setAttribute("aria-hidden", "true");
    overlay.style.cssText =
      "position:fixed;inset:0;z-index:99990;display:none;align-items:center;justify-content:center;" +
      "background:var(--overlay-backdrop, rgba(24,29,38,0.82));backdrop-filter:blur(7px);padding:24px;pointer-events:none;";

    const box = document.createElement("div");
    box.style.cssText =
      "max-width:460px;width:100%;padding:38px 28px;border-radius:20px;text-align:center;" +
      "background:var(--panel-bg-strong, rgba(59,66,82,0.96));border:2px dashed var(--accent, #88c0d0);" +
      "box-shadow:var(--shadow-menu, 0 24px 64px rgba(0,0,0,0.65));color:var(--text, #eceff4);font-family:'Segoe UI',system-ui,sans-serif;";
    box.innerHTML =
      '<div style="font-size:1.3rem;font-weight:800;margin-bottom:8px;background:linear-gradient(135deg,var(--accent,#88c0d0),var(--accent-2,#81a1c1));-webkit-background-clip:text;background-clip:text;color:transparent;">Drop .osr Replay to Render</div>' +
      '<div style="font-size:0.88rem;color:var(--text-dim,#d8dee9);line-height:1.5;">Release anywhere to open <strong>/render</strong> with your replay loaded.</div>';
    overlay.appendChild(box);
    document.body.appendChild(overlay);

    let depth = 0;
    const hasFiles = (e) => {
      const dt = e.dataTransfer;
      if (!dt || !dt.types) return false;
      for (let i = 0; i < dt.types.length; i++) {
        if (dt.types[i] === "Files") return true;
      }
      return false;
    };

    window.addEventListener("dragenter", (e) => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      depth++;
      overlay.style.display = "flex";
    });
    window.addEventListener("dragover", (e) => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      if (e.dataTransfer) e.dataTransfer.dropEffect = "copy";
    });
    window.addEventListener("dragleave", (e) => {
      if (depth <= 0) return;
      depth--;
      if (depth <= 0) {
        depth = 0;
        overlay.style.display = "none";
      }
    });
    window.addEventListener("drop", async (e) => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      depth = 0;
      overlay.style.display = "none";
      const files = e.dataTransfer && e.dataTransfer.files;
      if (!files || !files.length) return;
      let target = null;
      for (let i = 0; i < files.length; i++) {
        if (/\.osr$/i.test(files[i].name)) { target = files[i]; break; }
      }
      if (!target) {
        showGlobalToast("Please drop a .osr osu! replay file.", true);
        return;
      }
      try {
        await savePendingReplay(target);
        location.href = "/render?dropped=1";
      } catch (err) {
        showGlobalToast(err.message || "Failed to read replay file.", true);
      }
    });
  }

  function buildOsuLoginUrl(returnTo) {
    const target = returnTo || (location.pathname + location.search);
    return "/api/auth/osu/login?return_to=" + encodeURIComponent(target);
  }

  function closeAllNavMenus() {
    const authSlot = document.getElementById("osu-auth-slot");
    if (authSlot) {
      const menu = authSlot.querySelector(".osu-user-menu");
      const btn = authSlot.querySelector(".osu-user-btn");
      if (menu) menu.classList.remove("open");
      if (btn) btn.setAttribute("aria-expanded", "false");
    }
    const themeSlot = document.getElementById("osu-theme-slot");
    if (themeSlot) {
      const menu = themeSlot.querySelector(".osu-theme-menu");
      const btn = themeSlot.querySelector(".osu-theme-btn");
      if (menu) menu.classList.remove("open");
      if (btn) btn.setAttribute("aria-expanded", "false");
    }
  }

  function renderThemeSwitcher() {
    const slot = document.getElementById("osu-theme-slot");
    if (!slot) return;
    const wasOpen = Boolean(slot.querySelector(".osu-theme-menu.open"));
    slot.innerHTML = "";

    const cur = THEME_MAP[activeThemeId] || THEME_MAP[DEFAULT_THEME_ID];

    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "osu-theme-btn";
    btn.title = "Change site theme (" + cur.name + ")";
    btn.setAttribute("aria-haspopup", "menu");
    btn.setAttribute("aria-expanded", wasOpen ? "true" : "false");

    const dot = document.createElement("span");
    dot.className = "osu-theme-dot";
    dot.style.background = "linear-gradient(135deg, " + cur.preview.bg + " 48%, " + cur.preview.accent + " 52%)";
    btn.appendChild(dot);

    const lbl = document.createElement("span");
    lbl.className = "osu-theme-btn-label";
    lbl.textContent = cur.name;
    btn.appendChild(lbl);

    const caret = document.createElement("span");
    caret.style.cssText = "font-size:0.66rem;opacity:0.72;";
    caret.textContent = "▾";
    btn.appendChild(caret);

    const menu = document.createElement("div");
    menu.className = "osu-theme-menu" + (wasOpen ? " open" : "");
    menu.setAttribute("role", "menu");

    function appendSection(title, pillText, pillLocked) {
      const sec = document.createElement("div");
      sec.className = "osu-theme-section";
      const t = document.createElement("span");
      t.textContent = title;
      sec.appendChild(t);
      if (pillText) {
        const pill = document.createElement("span");
        pill.className = "osu-theme-section-pill" + (pillLocked ? " locked" : "");
        pill.textContent = pillText;
        sec.appendChild(pill);
      }
      menu.appendChild(sec);
    }

    function appendOption(theme) {
      const locked = theme.requiresLogin && !authState.logged_in;
      const isActive = theme.id === cur.id;

      const opt = document.createElement("button");
      opt.type = "button";
      opt.className = "osu-theme-opt" + (isActive ? " active" : "") + (locked ? " locked" : "");
      opt.setAttribute("role", "menuitemradio");
      opt.setAttribute("aria-checked", isActive ? "true" : "false");

      const left = document.createElement("div");
      left.className = "osu-theme-opt-left";

      const sw = document.createElement("span");
      sw.className = "osu-theme-swatches";
      sw.style.background = theme.preview.bg;
      const s1 = document.createElement("i");
      s1.style.background = theme.preview.accent;
      const s2 = document.createElement("i");
      s2.style.background = theme.preview.accent2;
      sw.appendChild(s1);
      sw.appendChild(s2);
      left.appendChild(sw);

      const txt = document.createElement("div");
      txt.className = "osu-theme-opt-text";
      const nm = document.createElement("span");
      nm.className = "osu-theme-opt-name";
      nm.textContent = theme.name;
      const sub = document.createElement("span");
      sub.className = "osu-theme-opt-sub";
      sub.textContent = theme.tagline;
      txt.appendChild(nm);
      txt.appendChild(sub);
      left.appendChild(txt);

      opt.appendChild(left);

      const tag = document.createElement("span");
      tag.className = "osu-theme-opt-tag";
      if (isActive) {
        tag.textContent = "✓";
      } else if (locked) {
        tag.textContent = "🔒";
        tag.title = "Log in with osu! to unlock";
      } else {
        tag.textContent = theme.mode === "light" ? "☀" : "☾";
        tag.style.opacity = "0.65";
      }
      opt.appendChild(tag);

      opt.addEventListener("click", (e) => {
        e.stopPropagation();
        if (locked) {
          showGlobalToast("Log in with osu! to unlock the " + theme.name + " theme!", true);
          return;
        }
        applyTheme(theme.id, { persist: true, syncServer: true, userInitiated: true });
        showGlobalToast("Theme: " + theme.name, false);
        closeAllNavMenus();
      });

      menu.appendChild(opt);
    }

    appendSection("Nord · Default", "Free", false);
    THEMES.filter((t) => !t.requiresLogin).forEach(appendOption);

    const div = document.createElement("div");
    div.className = "osu-theme-divider";
    menu.appendChild(div);

    appendSection(
      "osu! Account Themes",
      authState.logged_in ? "Unlocked" : "osu! Login",
      !authState.logged_in
    );
    THEMES.filter((t) => t.requiresLogin).forEach(appendOption);

    if (!authState.logged_in) {
      const cta = document.createElement("a");
      cta.className = "osu-theme-unlock-cta";
      cta.href = buildOsuLoginUrl();
      cta.textContent = "Log in with osu! to unlock all 7 themes →";
      menu.appendChild(cta);
    }

    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      const willOpen = !menu.classList.contains("open");
      closeAllNavMenus();
      menu.classList.toggle("open", willOpen);
      btn.setAttribute("aria-expanded", willOpen ? "true" : "false");
    });

    slot.appendChild(btn);
    slot.appendChild(menu);
  }

  function renderAuthWidget() {
    const slot = document.getElementById("osu-auth-slot");
    if (!slot) return;
    slot.innerHTML = "";

    if (!authState.logged_in || !authState.user) {
      const a = document.createElement("a");
      a.className = "osu-login-btn";
      a.href = buildOsuLoginUrl();
      a.textContent = "Log in with osu!";
      slot.appendChild(a);
      return;
    }

    const u = authState.user;
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "osu-user-btn";
    btn.setAttribute("aria-haspopup", "menu");
    btn.setAttribute("aria-expanded", "false");

    const img = document.createElement("img");
    img.className = "osu-user-avatar";
    img.src = u.avatar_url || ("https://a.ppy.sh/" + u.osu_user_id);
    img.alt = "";
    btn.appendChild(img);

    const nameSpan = document.createElement("span");
    nameSpan.textContent = u.username || ("User #" + u.osu_user_id);
    btn.appendChild(nameSpan);

    const caret = document.createElement("span");
    caret.style.cssText = "font-size:0.68rem;opacity:0.72;";
    caret.textContent = "▾";
    btn.appendChild(caret);

    const menu = document.createElement("div");
    menu.className = "osu-user-menu";
    menu.setAttribute("role", "menu");

    const head = document.createElement("div");
    head.className = "osu-user-menu-head";
    const bigImg = document.createElement("img");
    bigImg.src = img.src;
    bigImg.alt = "";
    head.appendChild(bigImg);

    const info = document.createElement("div");
    const nm = document.createElement("div");
    nm.className = "osu-user-menu-name";
    nm.textContent = u.username || ("User #" + u.osu_user_id);
    const meta = document.createElement("div");
    meta.className = "osu-user-menu-meta";
    const parts = [];
    if (u.country_code) parts.push(u.country_code);
    parts.push("#" + u.osu_user_id);
    if (u.linked_discord) parts.push("Discord linked");
    meta.textContent = parts.join(" · ");
    info.appendChild(nm);
    info.appendChild(meta);
    head.appendChild(info);
    menu.appendChild(head);

    const profLink = document.createElement("a");
    profLink.className = "osu-menu-link";
    profLink.href = "https://osu.ppy.sh/users/" + encodeURIComponent(String(u.osu_user_id));
    profLink.target = "_blank";
    profLink.rel = "noopener noreferrer";
    profLink.textContent = "osu! Profile ↗";
    menu.appendChild(profLink);

    const cfgLink = document.createElement("a");
    cfgLink.className = "osu-menu-link";
    cfgLink.href = "/config";
    cfgLink.textContent = "Account, Themes & Config";
    menu.appendChild(cfgLink);

    const logoutBtn = document.createElement("button");
    logoutBtn.type = "button";
    logoutBtn.className = "osu-menu-link danger";
    logoutBtn.textContent = "Log out";
    logoutBtn.addEventListener("click", () => {
      closeAllNavMenus();
      logoutOsu();
    });
    menu.appendChild(logoutBtn);

    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      const willOpen = !menu.classList.contains("open");
      closeAllNavMenus();
      menu.classList.toggle("open", willOpen);
      btn.setAttribute("aria-expanded", willOpen ? "true" : "false");
    });

    slot.appendChild(btn);
    slot.appendChild(menu);
  }

  function notifyAuthChanged() {
    renderAuthWidget();
    syncThemeWithAuth();
    window.dispatchEvent(new CustomEvent("osu-auth-change", { detail: authState }));
  }

  async function refreshAuthStatus() {
    try {
      const res = await fetch("/api/auth/me", { credentials: "same-origin", cache: "no-store" });
      const data = await res.json();
      if (res.ok && data && data.ok) {
        authState = {
          loaded: true,
          ok: true,
          logged_in: Boolean(data.logged_in && data.user),
          user: data.logged_in ? data.user : null,
        };
      } else {
        authState = { loaded: true, ok: false, logged_in: false, user: null };
      }
    } catch (_) {
      authState = { loaded: true, ok: false, logged_in: false, user: null };
    }
    notifyAuthChanged();
    return authState;
  }

  async function logoutOsu() {
    try {
      await fetch("/api/auth/logout", {
        method: "POST",
        credentials: "same-origin",
        cache: "no-store",
      });
      authState = { loaded: true, ok: true, logged_in: false, user: null };
      userPickedThemeThisSession = false;
      notifyAuthChanged();
      showGlobalToast("Logged out of osu!", false);
    } catch (_) {
      showGlobalToast("Failed to log out. Please try again.", true);
    }
  }

  function handleAuthQueryFeedback() {
    const params = new URLSearchParams(location.search);
    const authOk = params.get("auth");
    const authErr = params.get("auth_error");
    if (!authOk && !authErr) return null;

    params.delete("auth");
    params.delete("auth_error");
    const qs = params.toString();
    const cleanUrl = location.pathname + (qs ? "?" + qs : "") + location.hash;
    try {
      history.replaceState(null, "", cleanUrl);
    } catch (_) {}

    if (authErr) {
      const messages = {
        cancelled: "osu! login was cancelled.",
        oauth_denied: "osu! authorization was denied.",
        invalid_state: "Login session expired or state mismatch. Please try again.",
        missing_code: "osu! did not return an authorization code.",
        token_exchange: "osu! OAuth token exchange failed. Check your OAuth callback URL.",
        profile_fetch: "Could not load your osu! profile. Please try again.",
        db_error: "Failed saving login session. Please try again.",
        link_expired: "Discord verification link expired or already used. Run /link in Discord again.",
      };
      showGlobalToast(messages[authErr] || ("osu! login failed (" + authErr + ")."), true);
      return null;
    }
    return authOk === "ok" || authOk === "linked" ? authOk : null;
  }

  function initNavbarWidgets() {
    const nav =
      document.querySelector(".nav-actions") ||
      document.querySelector(".top-nav") ||
      document.querySelector(".nav-links");

    if (nav && !document.getElementById("osu-theme-slot")) {
      const themeSlot = document.createElement("div");
      themeSlot.id = "osu-theme-slot";
      themeSlot.className = "osu-theme-slot";
      nav.appendChild(themeSlot);
      renderThemeSwitcher();
    }

    if (nav && !document.getElementById("osu-auth-slot")) {
      const slot = document.createElement("div");
      slot.id = "osu-auth-slot";
      slot.className = "osu-auth-slot";
      nav.appendChild(slot);
      renderAuthWidget();
    }

    document.addEventListener("click", (e) => {
      const authSlot = document.getElementById("osu-auth-slot");
      const themeSlot = document.getElementById("osu-theme-slot");
      if (authSlot && authSlot.contains(e.target)) return;
      if (themeSlot && themeSlot.contains(e.target)) return;
      closeAllNavMenus();
    });

    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape") closeAllNavMenus();
    });

    const authFlag = handleAuthQueryFeedback();
    refreshAuthStatus().then((st) => {
      if (st.logged_in && st.user) {
        if (authFlag === "linked") {
          showGlobalToast("Verified & linked " + st.user.username + " to your Discord account!", false);
        } else if (authFlag === "ok") {
          showGlobalToast("Logged in as " + st.user.username + "!", false);
        }
      }
    });
  }

  window.OsuRenderShared = {
    MAX_OSR_BYTES,
    THEMES,
    DEFAULT_THEME_ID,
    savePendingReplay,
    loadPendingReplay,
    clearPendingReplay,
    arrayBufferToBase64,
    loadStoredConfig,
    saveStoredConfig,
    clearStoredConfig,
    showGlobalToast,
    buildOsuLoginUrl,
    refreshAuthStatus,
    logoutOsu,
    getAuthState: () => authState,
    getThemes: () => THEMES.slice(),
    getActiveTheme: () => THEME_MAP[activeThemeId] || THEME_MAP[DEFAULT_THEME_ID],
    isThemeUnlocked,
    applyTheme,
  };

  function bootShared() {
    initGlobalDropRedirect();
    initNavbarWidgets();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", bootShared);
  } else {
    bootShared();
  }
})();
