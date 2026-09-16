/**
 * Signing in with Hugging Face, from the page, with no secret anywhere near it.
 *
 * PKCE: the browser proves it started the flow, so the app needs no client secret and this page
 * can be read by anybody. `OAUTH_CLIENT_SECRET` is in the Space's environment and must never reach
 * here — `entrypoint.sh` substitutes only the id, which identifies the app and authorises nothing.
 *
 * **The client id is substituted by the server, not injected by the platform.** A static Space is
 * documented to provide `window.huggingface.variables.OAUTH_CLIENT_ID`, and for the console Space
 * it never did — through a rebuild, a privacy flip and a recreation. Its Dockerfile records that
 * afternoon. So this takes the path that depends on nothing that can silently stop happening:
 * `hf_oauth: true` puts the id in the container's environment and eight lines of `sh` put it in
 * the page. `?client_id=` still overrides, which is how a new app is tried before it is written
 * down anywhere.
 */
import { oauthHandleRedirectIfPresent, oauthLoginUrl } from "@huggingface/hub";

/** Substituted by `entrypoint.sh`. Left as the placeholder when the page is served any other way. */
const SERVED_CLIENT_ID = "{{OAUTH_CLIENT_ID}}";

/**
 * Where the sign-in is remembered.
 *
 * `localStorage` rather than `sessionStorage`: the Hugging Face redirect can come back in a new
 * tab, and a session store is empty there — which reads as a sign-in that silently did nothing.
 */
const REMEMBERED = "microduck-playground-oauth-v1";

export interface SignedIn {
  token: string;
  username: string;
}

function clientId(): string {
  const asked = new URLSearchParams(location.search).get("client_id");
  if (asked) return asked;
  const injected = (window as unknown as { huggingface?: { variables?: Record<string, string> } })
    .huggingface?.variables?.OAUTH_CLIENT_ID;
  if (injected) return injected;
  return SERVED_CLIENT_ID.startsWith("{{") ? "" : SERVED_CLIENT_ID;
}

/** The redirect the OAuth app must have registered, character for character. */
const REDIRECT = location.origin + location.pathname;

type Stored = { accessToken: string; userInfo?: { preferred_username?: string; name?: string } };

function nameOf(result: Stored): string {
  return result.userInfo?.preferred_username || result.userInfo?.name || "you";
}

/**
 * Consume a redirect if this page load is one, before anything else runs.
 *
 * Called first thing: a page holding a fresh `?code=` has one chance to exchange it, and anything
 * that re-renders or re-navigates first throws it away.
 */
export async function completeSignIn(): Promise<SignedIn | null> {
  let result: Stored | null = null;
  try {
    result = (await oauthHandleRedirectIfPresent()) as Stored | null;
  } catch {
    result = null;
  }
  if (result) {
    localStorage.setItem(REMEMBERED, JSON.stringify(result));
    history.replaceState(null, "", REDIRECT);
    return { token: result.accessToken, username: nameOf(result) };
  }

  const remembered = localStorage.getItem(REMEMBERED);
  if (!remembered) return null;
  try {
    const stored = JSON.parse(remembered) as Stored;
    return stored.accessToken ? { token: stored.accessToken, username: nameOf(stored) } : null;
  } catch {
    localStorage.removeItem(REMEMBERED);
    return null;
  }
}

/** Send the visitor to Hugging Face. This page is replaced, so nothing after it runs. */
export async function beginSignIn(): Promise<void> {
  const id = clientId();
  if (!id) {
    throw new Error(
      "This page has no Hugging Face app id, so it cannot sign anybody in. On a Space that " +
        "arrives from `hf_oauth: true` in the README; anywhere else, pass `?client_id=`.",
    );
  }
  location.href = await oauthLoginUrl({
    clientId: id,
    redirectUrl: REDIRECT,
    scopes: "openid profile",
  });
}

export function forgetSignIn(): void {
  localStorage.removeItem(REMEMBERED);
}

/** Whether signing in is possible at all here, so the page can say so rather than fail on a press. */
export function canSignIn(): boolean {
  return Boolean(clientId());
}
