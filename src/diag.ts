import { api } from "./api";

// Shared renderer diagnostics: mount line + window error capture into the
// persistent backend log, with per-signature dedupe so a render failing 60x/sec
// cannot flush the visibility/lifecycle history. Resource-load errors (where
// the event target is an element, not the window) are skipped — they are
// noise, not wedges. Each Tauri window has its own JS context, so the counts
// map is per-window by construction.
const counts = new Map<string, number>();

function report(tag: string, sig: string, snap: string) {
  const n = (counts.get(sig) ?? 0) + 1;
  counts.set(sig, n);
  if (n === 1 || n === 10 || n % 100 === 0) {
    api.debugLog(`${tag} error [x${n}] ${sig}${snap ? ` ${snap}` : ""}`).catch(() => {});
  }
}

export function installWindowDiagnostics(tag: string, snapshot: () => string = () => "") {
  const onErr = (e: ErrorEvent) => {
    if (e.target && e.target !== window) return;
    report(tag, `window.error message=${e.message} source=${e.filename}:${e.lineno}:${e.colno}`, snapshot());
  };
  const onRej = (e: PromiseRejectionEvent) => {
    let r = String(e.reason ?? "unknown");
    if (r.length > 200) r = r.slice(0, 200) + "…";
    report(tag, `unhandledrejection reason=${r}`, snapshot());
  };
  window.addEventListener("error", onErr);
  window.addEventListener("unhandledrejection", onRej);
  api
    .debugLog(`${tag} mounted href=${window.location.href} visibility=${document.visibilityState}`)
    .catch(() => {});
  return () => {
    window.removeEventListener("error", onErr);
    window.removeEventListener("unhandledrejection", onRej);
  };
}
