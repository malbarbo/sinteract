// The page of a player. The link of the player carries its token, as
// ?token=..., which the page keeps in the sessionStorage of the tab, so a
// reload finds it again. The page opens the WebSocket at /play of the same
// server, or at the URL of ?ws=... when one is given.

import type { MakeScreen } from "./screen.ts";
import { type Status, View } from "./view.ts";

const TOKEN_KEY = "sinteract.token";

const canvas = document.getElementById("stage") as HTMLCanvasElement;
const status = document.getElementById("status") as HTMLElement;
const message = document.getElementById("message") as HTMLElement;
const reconnect = document.getElementById("reconnect") as HTMLButtonElement;
const params = new URLSearchParams(location.search);

// Runs the page with the screen of `screen`, or a TsScreen.
export function runPage(screen?: MakeScreen): void {
  const token = params.get("token");
  if (token !== null) {
    sessionStorage.setItem(TOKEN_KEY, token);
    // Out of the address bar, so the link does not go on with a copy of it.
    params.delete("token");
    const query = params.size > 0 ? `?${params}` : "";
    history.replaceState(null, "", location.pathname + query + location.hash);
  }
  const view = new View(canvas, {
    onStatus: show,
    onError: (e) => console.warn("sinteract:", e),
    screen,
  });
  const start = () => connect(view);
  reconnect.onclick = start;
  start();
}

function connect(view: View): void {
  const url = params.get("ws") ?? playUrl(sessionStorage.getItem(TOKEN_KEY));
  if (url === null) {
    show({
      kind: "closed",
      code: 0,
      reason: "no token: open the link of a player",
    });
    return;
  }
  view.connect(url);
  canvas.focus();
}

function playUrl(token: string | null): string | null {
  if (token === null) return null;
  const url = new URL("/play", location.href);
  url.protocol = location.protocol === "https:" ? "wss:" : "ws:";
  url.searchParams.set("token", token);
  return url.href;
}

function show(s: Status): void {
  status.hidden = s.kind === "open";
  reconnect.hidden = s.kind !== "closed";
  switch (s.kind) {
    case "connecting":
      message.textContent = "connecting…";
      break;
    case "open":
      message.textContent = "";
      break;
    case "closed":
      message.textContent = s.reason
        ? `connection closed: ${s.reason}`
        : "connection closed";
      break;
  }
}
