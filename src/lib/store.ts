import { writable } from "svelte/store";
import type { AssetView, ScanSummary } from "./tauri";

export type Screen = "import" | "library" | "story" | "export" | "settings";

const CONSENT_KEY = "triptape-consent";

function readConsent(): boolean {
  try {
    return localStorage.getItem(CONSENT_KEY) === "1";
  } catch {
    return false; // no storage (SSR / private mode)
  }
}

export const screen = writable<Screen>("import");
export const project = writable<ScanSummary | null>(null);
export const assets = writable<AssetView[]>([]);
export const consentGiven = writable<boolean>(readConsent());

export function giveConsent() {
  try {
    localStorage.setItem(CONSENT_KEY, "1");
  } catch {
    /* private mode: consent lasts the session */
  }
  consentGiven.set(true);
}
