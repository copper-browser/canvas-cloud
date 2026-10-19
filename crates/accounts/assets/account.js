// canvas-cloud account page. Plain JS, no dependencies; every action is a call to the
// public REST API with the session token kept in sessionStorage (this tab only).
"use strict";

(function () {
  var TOKEN_KEY = "canvas-cloud.session";
  var info = null;
  var me = null;
  var liveToken = null; // { id, token } of a token created in this page view
  var $ = function (id) { return document.getElementById(id); };

  // ---- small helpers ----------------------------------------------------------------

  function show(el, on) { el.hidden = !on; }

  function session() {
    try { return sessionStorage.getItem(TOKEN_KEY); } catch (e) { return null; }
  }
  function setSession(token) {
    try {
      if (token) sessionStorage.setItem(TOKEN_KEY, token);
      else sessionStorage.removeItem(TOKEN_KEY);
    } catch (e) { /* storage unavailable: the page still works until reload */ }
  }

  function flash(message, isError) {
    var el = $("flash");
    el.textContent = message;
    el.classList.toggle("is-error", !!isError);
    el.setAttribute("role", isError ? "alert" : "status");
    show(el, !!message);
  }

  var MESSAGES = {
    invalid_credentials: "Email or password doesn’t match.",
    rate_limited: "Too many attempts. Wait a minute, then try again.",
    signup_closed: "Sign-up is closed on this server. Ask its administrator for an account.",
    signup_domain_not_allowed: "Sign-up on this server is limited to certain email domains.",
    email_taken: "An account with this email already exists. Sign in instead.",
    account_disabled: "This account is disabled. Contact the server’s administrator.",
    token_limit: "You already have the maximum number of tokens. Revoke one to create another.",
    password_not_set: "This account signs in with a provider and has no password. An administrator can set one.",
    session_required: "Sign in again to do this."
  };

  function messageFor(res, overrides) {
    var code = res.data && res.data.error;
    if (overrides && code && overrides[code]) return overrides[code];
    if (code && MESSAGES[code]) return MESSAGES[code];
    if (res.data && typeof res.data.message === "string") return res.data.message;
    if (res.status === 0) return "Can’t reach the server. Check your connection and try again.";
    return "Something went wrong (" + res.status + "). Try again.";
  }

  function api(method, path, body) {
    var headers = { "Accept": "application/json" };
    var token = session();
    if (token) headers["Authorization"] = "Bearer " + token;
    var init = { method: method, headers: headers, credentials: "omit", cache: "no-store" };
    if (body !== undefined) {
      headers["Content-Type"] = "application/json";
      init.body = JSON.stringify(body);
    }
    return fetch(path, init).then(function (resp) {
      return resp.text().then(function (text) {
        var data = null;
        try { data = text ? JSON.parse(text) : null; } catch (e) { data = null; }
        return { ok: resp.ok, status: resp.status, data: data };
      });
    }, function () {
      return { ok: false, status: 0, data: null };
    });
  }

  // An authenticated call; a 401 means the session ended.
  function authed(method, path, body) {
    return api(method, path, body).then(function (res) {
      if (res.status === 401 && session()) {
        setSession(null);
        signedOut("Your session ended. Sign in again.");
        throw new Error("signed out");
      }
      return res;
    });
  }

  function busy(button, on) {
    if (!button) return;
    button.disabled = on;
    if (on) button.setAttribute("aria-busy", "true");
    else button.removeAttribute("aria-busy");
  }

  function fieldError(form, errorEl, message, field) {
    errorEl.textContent = message || "";
    show(errorEl, !!message);
    Array.prototype.forEach.call(form.querySelectorAll("input"), function (i) {
      i.removeAttribute("aria-invalid");
    });
    if (field) {
      field.setAttribute("aria-invalid", "true");
      field.setAttribute("aria-describedby", (field.getAttribute("aria-describedby") || "").replace(errorEl.id, "").trim() + " " + errorEl.id);
      field.focus();
    }
  }

  function el(tag, attrs, children) {
    var node = document.createElement(tag);
    if (attrs) {
      Object.keys(attrs).forEach(function (k) {
        if (k === "text") node.textContent = attrs[k];
        else if (k === "className") node.className = attrs[k];
        else node.setAttribute(k, attrs[k]);
      });
    }
    (children || []).forEach(function (c) { if (c) node.appendChild(c); });
    return node;
  }

  var relative = (typeof Intl !== "undefined" && Intl.RelativeTimeFormat)
    ? new Intl.RelativeTimeFormat(undefined, { numeric: "auto" }) : null;
  var absolute = (typeof Intl !== "undefined" && Intl.DateTimeFormat)
    ? new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }) : null;

  function when(iso, empty) {
    if (!iso) return el("span", { className: "when", text: empty || "Never" });
    var d = new Date(iso);
    var text = d.toLocaleDateString();
    if (relative) {
      var secs = (d.getTime() - Date.now()) / 1000;
      var abs = Math.abs(secs);
      var units = [["year", 31536000], ["month", 2592000], ["week", 604800], ["day", 86400], ["hour", 3600], ["minute", 60]];
      text = relative.format(Math.round(secs), "second");
      for (var i = 0; i < units.length; i++) {
        if (abs >= units[i][1]) { text = relative.format(Math.round(secs / units[i][1]), units[i][0]); break; }
      }
      if (abs < 45) text = secs <= 0 ? "just now" : "in a moment";
    }
    var t = el("time", { datetime: iso, text: text, className: "when" });
    t.title = absolute ? absolute.format(d) : d.toString();
    return t;
  }

  function copyText(text, button) {
    function done(ok) {
      var label = button.textContent;
      button.textContent = ok ? "Copied" : "Press Ctrl+C";
      setTimeout(function () { button.textContent = label === "Copied" ? "Copy" : label; }, 1600);
    }
    if (navigator.clipboard && window.isSecureContext) {
      navigator.clipboard.writeText(text).then(function () { done(true); }, function () { done(false); });
      return;
    }
    var area = el("textarea", { readonly: "", "aria-hidden": "true", className: "sr-only" });
    area.value = text;
    document.body.appendChild(area);
    area.select();
    var ok = false;
    try { ok = document.execCommand("copy"); } catch (e) { ok = false; }
    document.body.removeChild(area);
    done(ok);
  }

  // ---- signed out ---------------------------------------------------------------------

  function showLogin(signup) {
    show($("login-form"), !signup);
    show($("signup-form"), !!signup);
    $("auth-title").textContent = signup ? "Create an account" : "Sign in";
    var open = !!(info && info.signup);
    show($("to-signup-line"), !signup && open);
    show($("to-login-line"), !!signup);
    show($("signup-closed"), !signup && !open);
  }

  function renderProviders() {
    var box = $("providers");
    box.textContent = "";
    var providers = (info && info.auth && info.auth.oidc) || [];
    providers.forEach(function (p) {
      var back = info.public_url + "/account";
      var href = info.public_url + p.start + "?redirect_uri=" + encodeURIComponent(back);
      box.appendChild(el("a", { className: "provider", href: href, text: "Continue with " + p.name }));
    });
    show(box, providers.length > 0);
    show($("divider"), providers.length > 0);
  }

  function signedOut(message) {
    me = null;
    liveToken = null;
    show($("loading"), false);
    show($("account"), false);
    show($("who"), false);
    show($("auth"), true);
    renderProviders();
    showLogin(false);
    if (message) flash(message, true);
  }

  function authSubmit(form, path, body, errorEl, overrides) {
    var button = form.querySelector("button[type=submit]");
    busy(button, true);
    return api("POST", path, body).then(function (res) {
      busy(button, false);
      if (!res.ok || !res.data || !res.data.token) {
        fieldError(form, errorEl, messageFor(res, overrides));
        return;
      }
      setSession(res.data.token);
      form.reset();
      fieldError(form, errorEl, "");
      flash("");
      loadAccount();
    });
  }

  function onLogin(ev) {
    ev.preventDefault();
    var form = ev.currentTarget;
    var email = $("login-email");
    var password = $("login-password");
    if (!email.value.trim()) return fieldError(form, $("login-error"), "Enter your email.", email);
    if (!password.value) return fieldError(form, $("login-error"), "Enter your password.", password);
    authSubmit(form, "/v1/auth/login", { email: email.value.trim(), password: password.value }, $("login-error"));
  }

  function onSignup(ev) {
    ev.preventDefault();
    var form = ev.currentTarget;
    var email = $("signup-email");
    var password = $("signup-password");
    var name = $("signup-name").value.trim();
    if (!/^[^\s@]+@[^\s@]+$/.test(email.value.trim())) {
      return fieldError(form, $("signup-error"), "Enter a valid email address.", email);
    }
    if (password.value.length < 10) {
      return fieldError(form, $("signup-error"), "Use at least 10 characters.", password);
    }
    var body = { email: email.value.trim(), password: password.value };
    if (name) body.display_name = name;
    authSubmit(form, "/v1/auth/signup", body, $("signup-error"));
  }

  // ---- signed in ----------------------------------------------------------------------

  // A canvas opens in the web app only when `web_app_url` has a `{canvas}` placeholder (the
  // `{token}` share-link placeholder core requires is dropped here).
  function openUrl(id) {
    var app = info && info.web && info.web.app_url;
    if (!app || app.indexOf("{canvas}") === -1) return null;
    return app.split("{canvas}").join(encodeURIComponent(id)).split("{token}").join("");
  }

  function renderCanvases(list) {
    var body = $("canvases").querySelector("tbody");
    body.textContent = "";
    var anyLink = false;
    list.forEach(function (c) {
      var url = openUrl(c.id);
      if (url) anyLink = true;
      var name = el("td", { className: "name" }, [
        document.createTextNode(c.name || "Untitled"),
        url ? null : el("span", { className: "sub id", text: c.id })
      ]);
      var kind = c.kind === "shared" ? "Shared" : "Personal";
      if (c.role && c.role !== "owner") kind += " · " + c.role;
      var action = el("td", { className: "action" }, [
        url ? el("a", { href: url, text: "Open", "aria-label": "Open " + (c.name || "canvas") }) : null
      ]);
      body.appendChild(el("tr", null, [
        name,
        el("td", { className: "meta" }, [el("span", { className: "tag", text: kind })]),
        el("td", { className: "num meta", "data-label": "Members", text: String(c.member_count) }),
        el("td", { className: "meta", "data-label": "Updated" }, [when(c.updated_at)]),
        action
      ]));
    });
    $("canvas-count").textContent = list.length ? String(list.length) : "";
    show($("canvases-wrap"), list.length > 0);
    show($("canvases-empty"), list.length === 0);
    show($("open-hint"), list.length > 0 && !anyLink);
  }

  function renderTokens(list) {
    var body = $("tokens").querySelector("tbody");
    body.textContent = "";
    list.forEach(function (t) {
      var revoke = el("button", { type: "button", className: "danger", text: "Revoke", "aria-label": "Revoke " + t.name });
      revoke.addEventListener("click", function () { revokeToken(t, revoke); });
      var created = el("span", { className: "sub" });
      created.textContent = "created " + new Date(t.created_at).toLocaleDateString();
      body.appendChild(el("tr", null, [
        el("td", { className: "name" }, [document.createTextNode(t.name), created]),
        el("td", { className: "meta" }, [el("span", { className: "tag" + (t.scope === "write" ? " is-write" : ""), text: t.scope === "write" ? "Read and write" : "Read only" })]),
        el("td", { className: "meta", "data-label": "Last used" }, [when(t.last_used_at, "Never")]),
        el("td", { className: "meta", "data-label": "Expires" }, [when(t.expires_at, "Never")]),
        el("td", { className: "action" }, [revoke])
      ]));
    });
    var limit = $("token-count");
    limit.textContent = list.length ? list.length + " of 50" : "";
    show($("tokens-wrap"), list.length > 0);
    show($("tokens-empty"), list.length === 0);
  }

  function renderSnippet() {
    var code = $("mcp-snippet");
    var base = (info && info.public_url) || location.origin;
    var token = liveToken ? liveToken.token : "<token>";
    var before = "{\n  \"mcpServers\": {\n    \"canvas-cloud\": {\n      \"url\": " + JSON.stringify(base + "/mcp") +
      ",\n      \"headers\": {\n        \"Authorization\": \"Bearer ";
    var after = "\"\n      }\n    }\n  }\n}";
    code.textContent = "";
    code.appendChild(document.createTextNode(before));
    code.appendChild(el("span", { className: "tok", text: token }));
    code.appendChild(document.createTextNode(after));
    var live = !!liveToken;
    $("mcp-snippet").closest(".snippet").classList.toggle("is-live", live);
    $("snippet-label").textContent = live ? "MCP client config · includes your new token" : "MCP client config";
    var hint = $("snippet-hint");
    hint.textContent = "";
    if (live) {
      hint.appendChild(document.createTextNode("This config contains your new token. Keep it private, like a password."));
    } else {
      hint.appendChild(document.createTextNode("Replace "));
      hint.appendChild(el("code", { text: "<token>" }));
      hint.appendChild(document.createTextNode(" with an access token from above."));
    }
  }

  function loadTokens() {
    return authed("GET", "/v1/tokens").then(function (res) {
      if (res.ok) renderTokens(res.data.tokens || []);
      else flash(messageFor(res), true);
    });
  }

  function loadCanvases() {
    return authed("GET", "/v1/canvases").then(function (res) {
      if (res.ok) renderCanvases(Array.isArray(res.data) ? res.data : (res.data && res.data.canvases) || []);
      else flash(messageFor(res), true);
    });
  }

  function renderMe() {
    var name = me.user.display_name || me.user.email;
    $("who-email").textContent = name;
    $("greeting").textContent = "Your account";
    $("greeting-sub").textContent = "Signed in as " + me.user.email + ".";
    $("profile-name").value = me.user.display_name || "";
    $("password-username").value = me.user.email;
  }

  function loadAccount() {
    show($("loading"), true);
    return authed("GET", "/v1/auth/me").then(function (res) {
      if (!res.ok) {
        setSession(null);
        signedOut(messageFor(res));
        return;
      }
      me = res.data;
      renderMe();
      show($("auth"), false);
      show($("loading"), false);
      show($("who"), true);
      show($("account"), true);
      renderSnippet();
      return Promise.all([loadCanvases(), loadTokens()]);
    }).catch(function () { /* already handled (signed out) */ });
  }

  function onCreateToken(ev) {
    ev.preventDefault();
    var form = ev.currentTarget;
    var name = $("token-name");
    if (!name.value.trim()) return fieldError(form, $("token-error"), "Give the token a name, so you can recognise it later.", name);
    var body = { name: name.value.trim(), scope: $("token-scope").value };
    var days = $("token-expiry").value;
    if (days) body.expires_in_days = parseInt(days, 10);
    var button = form.querySelector("button[type=submit]");
    busy(button, true);
    authed("POST", "/v1/tokens", body).then(function (res) {
      busy(button, false);
      if (!res.ok) return fieldError(form, $("token-error"), messageFor(res));
      fieldError(form, $("token-error"), "");
      name.value = "";
      liveToken = { id: res.data.id, token: res.data.token };
      $("new-token-value").textContent = res.data.token;
      $("new-token-title").textContent = "Copy “" + res.data.name + "” now. It won’t be shown again.";
      show($("new-token"), true);
      renderSnippet();
      $("new-token").querySelector("button").focus();
      loadTokens();
    }).catch(function () { busy(button, false); });
  }

  function revokeToken(t, button) {
    if (!window.confirm("Revoke “" + t.name + "”? Anything using it stops working immediately.")) return;
    busy(button, true);
    authed("DELETE", "/v1/tokens/" + encodeURIComponent(t.id)).then(function (res) {
      busy(button, false);
      if (!res.ok && res.status !== 404) return flash(messageFor(res), true);
      if (liveToken && liveToken.id === t.id) {
        liveToken = null;
        show($("new-token"), false);
        renderSnippet();
      }
      flash("Revoked “" + t.name + "”.");
      loadTokens().then(function () { $("token-name").focus(); });
    }).catch(function () { busy(button, false); });
  }

  function onSaveName(ev) {
    ev.preventDefault();
    var form = ev.currentTarget;
    var input = $("profile-name");
    if (!input.value.trim()) return fieldError(form, $("name-error"), "Enter a display name.", input);
    var button = form.querySelector("button[type=submit]");
    busy(button, true);
    authed("PATCH", "/v1/auth/me", { display_name: input.value.trim() }).then(function (res) {
      busy(button, false);
      if (!res.ok) return fieldError(form, $("name-error"), messageFor(res));
      fieldError(form, $("name-error"), "");
      me = res.data;
      renderMe();
      flash("Display name saved.");
    }).catch(function () { busy(button, false); });
  }

  function onChangePassword(ev) {
    ev.preventDefault();
    var form = ev.currentTarget;
    var current = $("current-password");
    var next = $("new-password");
    if (!current.value) return fieldError(form, $("password-error"), "Enter your current password.", current);
    if (next.value.length < 10) return fieldError(form, $("password-error"), "Use at least 10 characters.", next);
    var button = form.querySelector("button[type=submit]");
    busy(button, true);
    authed("POST", "/v1/auth/password", { current: current.value, "new": next.value }).then(function (res) {
      busy(button, false);
      if (!res.ok) {
        var wrong = res.data && res.data.error === "invalid_credentials";
        return fieldError(form, $("password-error"),
          messageFor(res, { invalid_credentials: "Your current password is wrong." }), wrong ? current : null);
      }
      form.reset();
      $("password-username").value = me.user.email;
      fieldError(form, $("password-error"), "");
      var n = res.data.revoked_sessions || 0;
      flash("Password changed." + (n ? " Signed out " + n + " other session" + (n === 1 ? "" : "s") + "." : ""));
    }).catch(function () { busy(button, false); });
  }

  function onSignOut() {
    var button = $("sign-out");
    busy(button, true);
    api("POST", "/v1/auth/logout").then(function () {
      busy(button, false);
      setSession(null);
      signedOut();
      flash("Signed out.");
      $("login-email").focus();
    });
  }

  // ---- start --------------------------------------------------------------------------

  function readFragment() {
    if (!location.hash || location.hash.length < 2) return;
    var params = new URLSearchParams(location.hash.slice(1));
    var token = params.get("token");
    var error = params.get("error");
    history.replaceState(null, "", location.pathname + location.search);
    if (token) setSession(token);
    if (error) {
      var text = params.get("error_description") || "Sign-in failed";
      flash(text.charAt(0).toUpperCase() + text.slice(1) + ". Try again, or sign in another way.", true);
    }
  }

  function wire() {
    $("login-form").addEventListener("submit", onLogin);
    $("signup-form").addEventListener("submit", onSignup);
    $("to-signup").addEventListener("click", function () { showLogin(true); $("signup-email").focus(); });
    $("to-login").addEventListener("click", function () { showLogin(false); $("login-email").focus(); });
    $("token-form").addEventListener("submit", onCreateToken);
    $("name-form").addEventListener("submit", onSaveName);
    $("password-form").addEventListener("submit", onChangePassword);
    $("sign-out").addEventListener("click", onSignOut);
    Array.prototype.forEach.call(document.querySelectorAll("[data-copy]"), function (b) {
      b.addEventListener("click", function () { copyText($(b.getAttribute("data-copy")).textContent, b); });
    });
    Array.prototype.forEach.call(document.querySelectorAll("[data-reveal]"), function (b) {
      b.addEventListener("click", function () {
        var input = $(b.getAttribute("data-reveal"));
        var reveal = input.type === "password";
        input.type = reveal ? "text" : "password";
        b.textContent = reveal ? "Hide" : "Show";
        b.setAttribute("aria-pressed", reveal ? "true" : "false");
        b.setAttribute("aria-label", reveal ? "Hide password" : "Show password");
      });
    });
  }

  function start() {
    wire();
    readFragment();
    api("GET", "/v1/info").then(function (res) {
      info = res.ok && res.data ? res.data : { public_url: location.origin, signup: false, auth: { oidc: [] }, web: {} };
      $("host").textContent = (info.public_url || location.origin).replace(/^[a-z]+:\/\//, "");
      if (info.version) $("version").textContent = "canvas-cloud " + info.version;
      if (session()) loadAccount();
      else signedOut();
    });
  }

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start);
  else start();
})();
