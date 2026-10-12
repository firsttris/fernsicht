/**
 * The viewer served by a host comes over plain HTTP in the LAN. Chrome and
 * Edge keep Keyboard Lock (system keys in fullscreen) and raw pointer
 * movement (`pointerrawupdate`) to secure pages, but treat an origin as
 * secure when the user lists it in a flag (D7, until the host has a real
 * certificate). Firefox and Safari have neither feature, so nothing to
 * unlock there.
 */

/** What decides the hint, read from the page (see {@link pageEnv}). */
export interface PageEnv {
  secure: boolean;
  origin: string;
  userAgent: string;
}

/** Where a Chromium browser lists the origin, and the origin to list. */
export interface SecureHint {
  flag: string;
  origin: string;
}

const FLAG = "unsafely-treat-insecure-origin-as-secure";

/** The hint for this page; `null` where it is secure or nothing helps. */
export function secureHint(env: PageEnv): SecureHint | null {
  if (env.secure) return null;
  const ua = env.userAgent;
  // Chrome on iOS is WebKit: no flags. A phone has no system keys to
  // pass on, nor a mouse for games.
  if (!/Chrome\//.test(ua) || /CriOS|EdgiOS|Android|Mobile/.test(ua)) return null;
  const scheme = /Edg\//.test(ua) ? "edge" : "chrome";
  return { flag: `${scheme}://flags/#${FLAG}`, origin: env.origin };
}

export function pageEnv(): PageEnv {
  return {
    secure: window.isSecureContext === true,
    origin: window.location.origin,
    userAgent: navigator.userAgent,
  };
}
