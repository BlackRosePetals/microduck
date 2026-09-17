/**
 * The playground: pick a trick, watch your duck do it.
 *
 * **Written for a ten-year-old**, which is a constraint on the whole file and not a coat of paint.
 * Nothing on the page names a method, a transport, a socket or a schema. A trick has a name, a
 * sentence about what it does, and one button. What the four calls are, which lane they take and
 * why a refusal happened lives in *What just happened?* at the bottom, which is where you go when
 * something breaks rather than when you want to see a duck bow.
 *
 * The press has stages, and they are shown on the card that was pressed rather than in a line
 * somewhere else on the page:
 *
 *     getting it → putting it on your duck → waiting for your duck → doing it
 *
 * and the rest of the page is inert while one is in flight. A robot can only do one thing at a
 * time, so a page that accepted a second press would be promising something it cannot keep.
 */
import { beginSignIn, canSignIn, completeSignIn, forgetSignIn, localHint, type SignedIn } from "./auth";
import { howLong, isATrick, needsALength, notATrick, readHub, skillFor, type Policy } from "./hub";
import { listDucks, RpcError, Session, type Robot } from "./rendezvous";
import "./style.css";

/** How long to hold a trick that has no length of its own. Perpetual means "until told otherwise". */
const HOLD_SECONDS = 3;

/** A download over the duck's own wifi, not a question about state. */
const FETCH_TIMEOUT = 180_000;

/**
 * How long to wait for a duck that is going back to its standing pose.
 *
 * Putting a trick on a duck makes it reload, and a reloading duck refuses to do anything until it
 * is standing again — so the press that installs is the press that gets refused. `homed` on
 * `robot.policies` is the flag to wait on; a duck too old to publish it sends nothing, and then
 * there is nothing to wait for.
 */
const HOME_TIMEOUT = 15_000;

interface State {
  signedIn: SignedIn | null;
  ducks: Robot[];
  chosen: string | null;
  session: Session | null;
  duckName: string | null;
  onTheDuck: string[];
  policies: Policy[];
  trouble: string | null;
  busy: string | null;
  stage: string | null;
  said: string | null;
  log: string[];
}

const state: State = {
  signedIn: null,
  ducks: [],
  chosen: null,
  session: null,
  duckName: null,
  onTheDuck: [],
  policies: [],
  trouble: null,
  busy: null,
  stage: null,
  said: null,
  log: [],
};

function note(line: string): void {
  const stamp = new Date().toLocaleTimeString();
  state.log.push(`${stamp}  ${line}`);
  if (state.log.length > 400) state.log.shift();
  const panel = document.querySelector<HTMLPreElement>("#log");
  if (panel) {
    panel.textContent = state.log.join("\n");
    panel.scrollTop = panel.scrollHeight;
  }
}

const sleep = (ms: number): Promise<void> => new Promise((done) => window.setTimeout(done, ms));

// ── talking to the duck ──────────────────────────────────────────────────────

async function connect(): Promise<void> {
  const token = state.signedIn?.token;
  const peerId = state.chosen;
  if (!token || !peerId) return;
  const duck = state.ducks.find((d) => d.peerId === peerId);
  state.busy = "connecting";
  render();
  try {
    const session = new Session(token, peerId, "microduck-policy-playground", note);
    await session.start();
    state.session = session;
    state.duckName = duck?.name ?? "your duck";
    state.said = `${state.duckName} is ready.`;
    await readTheDuck();
  } catch (e) {
    state.said = `Could not reach your duck. ${e instanceof Error ? e.message : String(e)}`;
    note(String(e));
  } finally {
    state.busy = null;
    render();
  }
}

async function disconnect(): Promise<void> {
  await state.session?.stop();
  state.session = null;
  state.duckName = null;
  state.onTheDuck = [];
  state.said = "Let go of your duck.";
  render();
}

/** What the duck already knows, which is the shelf at the top of the page. */
async function readTheDuck(): Promise<void> {
  const session = state.session;
  if (!session) return;
  try {
    const policies = (await session.call("robot.policies")) as Record<string, unknown>;
    const skills = Array.isArray(policies.skills) ? (policies.skills as string[]) : [];
    const built = (await session.call("robot.skills")) as Record<string, unknown>;
    const builtIn = Array.isArray(built.built_in) ? (built.built_in as string[]) : [];
    state.onTheDuck = [...skills, ...builtIn.filter((n) => !skills.includes(n))];
  } catch (e) {
    note(`could not read the duck: ${e}`);
  }
}

/**
 * Wait until the duck will accept a trick.
 *
 * `accepted: false` with a reason is a normal answer, not a failure — and "still going to its home
 * pose" is the one reason that clears by itself. Asking `homed` rather than reading the sentence
 * means the wait ends when the duck is ready instead of when a timer says so.
 */
async function waitForHome(): Promise<void> {
  const session = state.session;
  if (!session) return;
  const deadline = Date.now() + HOME_TIMEOUT;
  while (Date.now() < deadline) {
    let homed: unknown;
    try {
      homed = ((await session.call("robot.policies")) as Record<string, unknown>).homed;
    } catch {
      return;
    }
    if (homed === undefined || homed === null) {
      note("this duck does not say whether it is standing yet, so there is nothing to wait for");
      return;
    }
    if (homed) return;
    await sleep(500);
  }
  note(`gave up waiting for the duck to stand after ${HOME_TIMEOUT / 1000}s`);
}

/** `accepted: false` carries the reason; `accepted: true` with one means it was already done. */
function refusal(result: unknown): string | null {
  if (typeof result !== "object" || result === null || !("accepted" in result)) return null;
  const answer = result as { accepted?: unknown; reason?: unknown };
  if (answer.accepted) return null;
  return String(answer.reason ?? "your duck said no, without saying why");
}

async function teachAndDo(policy: Policy): Promise<void> {
  const session = state.session;
  if (!session) return;
  const blocked = notATrick(policy);
  if (blocked) {
    state.said = blocked;
    render();
    return;
  }

  state.busy = policy.key;
  try {
    state.stage = "Getting it…";
    render();
    const params: Record<string, unknown> = { repo: policy.repo };
    if (policy.file) params.file = policy.file;
    const fetched = (await session.call("policy.fetch", params, FETCH_TIMEOUT)) as Record<string, unknown>;

    state.stage = "Putting it on your duck…";
    render();
    const skill = skillFor(fetched, HOLD_SECONDS);
    const added = await session.call("robot.setSkill", skill);
    const notAdded = refusal(added);
    if (notAdded) throw new Error(notAdded);

    // Accepted is not installed: `setSkill` triggers a reload, and a reload that failed says so
    // here and nowhere else.
    const after = (await session.call("robot.policies")) as Record<string, unknown>;
    if (after.change_error) throw new Error(String(after.change_error));

    state.stage = "Waiting for your duck…";
    render();
    await waitForHome();

    state.stage = "Doing it!";
    render();
    const name = String(skill.name);
    const ran = await session.call("robot.do", { skill: name });
    const notRun = refusal(ran);
    state.said = notRun
      ? `${policy.name} is on your duck, but it would not do it: ${notRun}`
      : `${policy.name}! ${state.duckName ?? "Your duck"} is doing it.`;
    await readTheDuck();
  } catch (e) {
    const why = e instanceof RpcError ? e.message : e instanceof Error ? e.message : String(e);
    state.said = `${policy.name} did not work: ${why}`;
  } finally {
    state.busy = null;
    state.stage = null;
    render();
  }
}

async function doAgain(name: string): Promise<void> {
  const session = state.session;
  if (!session) return;
  state.busy = `again:${name}`;
  state.stage = "Doing it!";
  render();
  try {
    await waitForHome();
    const ran = await session.call("robot.do", { skill: name });
    const notRun = refusal(ran);
    state.said = notRun ? `It would not do ${name}: ${notRun}` : `${name}!`;
  } catch (e) {
    state.said = `It would not do ${name}: ${e instanceof Error ? e.message : String(e)}`;
  } finally {
    state.busy = null;
    state.stage = null;
    render();
  }
}

// ── the page ─────────────────────────────────────────────────────────────────

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function card(policy: Policy): HTMLElement {
  // Either reason not to offer a button: the daemon drives it, or it has no ending. The card says
  // which, in the same place, because to a reader they are the same answer — "not this one".
  const blocked =
    notATrick(policy) ??
    (isATrick(policy) ? null : "This one keeps going rather than finishing, so it is not a trick.");
  const node = el("article", `card${blocked ? " card-blocked" : ""}`);
  node.append(el("h3", "card-name", policy.name));
  node.append(el("p", "card-what", policy.description ?? "Nobody wrote down what this one does."));

  const facts = el("p", "card-facts");
  facts.append(el("span", "chip", howLong(policy)));
  if (policy.official) facts.append(el("span", "chip chip-official", "made by Pollen"));
  if (needsALength(policy) && !blocked) facts.append(el("span", "chip", `held for ${HOLD_SECONDS}s`));
  node.append(facts);

  if (blocked) {
    node.append(el("p", "card-blocked-why", blocked));
    return node;
  }

  const mine = state.busy === policy.key;
  const button = el("button", "go", mine ? (state.stage ?? "…") : "Teach my duck");
  button.disabled = !state.session || state.busy !== null;
  if (mine) button.classList.add("go-busy");
  button.addEventListener("click", () => void teachAndDo(policy));
  node.append(button);
  return node;
}

function header(): HTMLElement {
  const bar = el("header", "bar");
  bar.append(el("h1", "title", "🦆 Duck tricks"));

  const right = el("div", "bar-right");
  if (!state.signedIn) {
    const button = el("button", "primary", "Sign in with Hugging Face");
    button.disabled = !canSignIn();
    button.addEventListener("click", () => void beginSignIn());
    right.append(button);
    const hint = localHint();
    if (hint) right.append(el("span", "chip chip-bad", "no app id — see below"));
    else if (!canSignIn()) right.append(el("span", "chip chip-bad", "no app id on this page"));
  } else if (!state.session) {
    const picker = el("select", "picker");
    if (state.ducks.length === 0) {
      picker.append(new Option("no ducks awake", ""));
      picker.disabled = true;
    }
    for (const duck of state.ducks) {
      picker.append(new Option(duck.busy ? `${duck.name} (busy)` : duck.name, duck.peerId));
    }
    picker.value = state.chosen ?? "";
    picker.addEventListener("change", () => {
      state.chosen = picker.value || null;
    });
    right.append(picker);

    const find = el("button", "", "Look again");
    find.addEventListener("click", () => void findDucks());
    right.append(find);

    const join = el("button", "primary", state.busy === "connecting" ? "Connecting…" : "Wake it up");
    join.disabled = !state.chosen || state.busy !== null;
    join.addEventListener("click", () => void connect());
    right.append(join);
  } else {
    right.append(el("span", "chip chip-live", `${state.duckName} is listening`));
    const leave = el("button", "", "Let go");
    leave.disabled = state.busy !== null;
    leave.addEventListener("click", () => void disconnect());
    right.append(leave);
  }
  bar.append(right);
  return bar;
}

function shelf(): HTMLElement | null {
  if (!state.session || state.onTheDuck.length === 0) return null;
  const box = el("section", "shelf");
  box.append(el("h2", "", "Already on your duck"));
  const row = el("div", "shelf-row");
  for (const name of state.onTheDuck) {
    const button = el("button", "known", state.busy === `again:${name}` ? (state.stage ?? "…") : name);
    button.disabled = state.busy !== null;
    button.addEventListener("click", () => void doAgain(name));
    row.append(button);
  }
  box.append(row);
  return box;
}

function render(): void {
  const root = document.querySelector<HTMLDivElement>("#app");
  if (!root) return;
  root.replaceChildren();
  root.append(header());

  if (state.said) {
    const said = el("p", "said", state.said);
    root.append(said);
  }

  if (!state.session) {
    root.append(
      el(
        "p",
        "hint",
        state.signedIn
          ? "Pick your duck and wake it up, then choose a trick."
          : (localHint() ??
              "Sign in to find your duck. You will only see robots your own account owns."),
      ),
    );
  }

  const known = shelf();
  if (known) root.append(known);

  const tricks = el("section", "tricks");
  tricks.append(el("h2", "", "Tricks you can add"));
  if (state.trouble) tricks.append(el("p", "hint", state.trouble));
  const grid = el("div", "grid");
  for (const policy of state.policies.filter(isATrick)) grid.append(card(policy));
  tricks.append(grid);
  root.append(tricks);

  // **Listed rather than hidden, and not offered as something to press.** These are the gaits and
  // the postures — the things a duck moves *with* rather than things it can show you. Leaving them
  // in the same grid put "alpha walking" beside "polite bow" with an identical button, which is a
  // page telling a child they are the same kind of thing.
  const rest = state.policies.filter((p) => !isATrick(p));
  if (rest.length) {
    const box = el("details", "rest");
    box.append(el("summary", "", `Part of how your duck moves (${rest.length})`));
    box.append(
      el("p", "hint", "These are not tricks. They are how your duck walks, stands and picks things up."),
    );
    const list = el("div", "grid");
    for (const policy of rest) list.append(card(policy));
    box.append(list);
    root.append(box);
  }

  const details = el("details", "log-box");
  details.append(el("summary", "", "What just happened?"));
  const pre = el("pre", "");
  pre.id = "log";
  pre.textContent = state.log.join("\n");
  details.append(pre);
  root.append(details);
}

async function findDucks(): Promise<void> {
  const token = state.signedIn?.token;
  if (!token) return;
  try {
    const { ducks, others } = await listDucks(token);
    state.ducks = ducks;
    state.chosen = ducks[0]?.peerId ?? null;
    if (ducks.length === 0) {
      state.said = others.length
        ? `No ducks awake. ${others.length} other robot(s) are, but they are not ducks.`
        : "No ducks awake. Turn yours on, and give it a minute to say hello.";
    } else {
      state.said = null;
    }
  } catch (e) {
    state.said = e instanceof Error ? e.message : String(e);
    note(String(e));
  }
  render();
}

async function main(): Promise<void> {
  render();

  state.signedIn = await completeSignIn();
  if (state.signedIn) note(`signed in as ${state.signedIn.username}`);
  render();

  const { policies, trouble } = await readHub();
  state.policies = policies;
  state.trouble = trouble;
  note(`${policies.length} tricks on the Hub`);
  render();

  if (state.signedIn) await findDucks();
}

void main();

// Signing out is not a button anybody asked for, but a stale token is a 401 nobody can explain.
// `?forget` is the escape hatch, named in the log rather than on the page.
if (new URLSearchParams(location.search).has("forget")) {
  forgetSignIn();
  location.replace(location.pathname);
}
