// @description Removes Nitro upsells: empties promo, offer and quest fetches, hides the gift button, and auto-hides upsell UI by id pattern
// @version 0.2.0
// @author Fluxenite
// @permissions discord hooks messages

(() => {
// nonitroads: strips Nitro upsells three ways.
//   1. API: promo, trial-offer and quest lookups are emptied so popups have nothing to show.
//   2. UI by id: the gift button and any ids you list are hidden.
//   3. UI by pattern: every view whose accessibility id matches a regex is hidden as it appears.

const DEFAULT_PATTERN = "nitro|upsell|premium.?(?:promo|banner|upgrade|tier)|boost.?(?:promo|upsell)|upgrade.?(?:banner|cta)";
const SEEN_LIMIT = 1000;

// Anything that fails at load is logged and listed by /nonitroads instead of stopping the plugin.
const loadErrors: string[] = [];
const safe = (name: string, fn: () => void) => {
  try {
    fn();
  } catch (err) {
    loadErrors.push(`${name}: ${err}`);
    phoenix.log.error(`nonitroads ${name} failed: ${err}`);
  }
};

const settings = phoenix.settings.define({
  offers: {
    type: "switch",
    title: "Block Nitro offers",
    description: "Empties promo and trial-offer lookups so their popups and banners have nothing to show",
    default: true,
  },
  quests: {
    type: "switch",
    title: "Block quests",
    description: "Empties the quest list, which drives the promo banners and popups",
    default: true,
  },
  hideGift: {
    type: "switch",
    title: "Hide gift button",
    description: "Removes the Nitro gift button from the message box",
    default: true,
    onChange: () => applyHides(),
  },
  extraIds: {
    type: "text",
    multiline: true,
    title: "Extra UI ids to hide",
    description: "One accessibility id per line, for any upsell button or banner left over",
    default: "",
    onChange: () => applyHides(),
  },
  autoHide: {
    type: "switch",
    title: "Auto-hide by pattern",
    description: "Hides every view whose accessibility id matches the pattern below as it is created",
    default: true,
    onChange: () => rescan(),
  },
  autoPattern: {
    type: "text",
    title: "Auto-hide pattern",
    description: "A case-insensitive regex tested against each view's accessibility id",
    default: DEFAULT_PATTERN,
    onChange: () => rescan(),
  },
  debug: {
    type: "switch",
    title: "Log UI ids and API paths",
    description: "Writes every new accessibility id and API request path to the plugin log, to find what to block",
    default: false,
  },
  status: {
    type: "button",
    title: "Show status",
    onPress: () => {
      phoenix.ui
        .showSheet({
          title: "nonitroads status",
          monospace: true,
          text:
            `Blocked: ${stats()}\n` +
            `UI ids seen: ${seen.size}, auto-hidden: ${autoHidden.size}, API paths seen: ${seenPaths.size}\n` +
            `Load errors: ${loadErrors.length ? "\n" + loadErrors.join("\n") : "none"}`,
        })
        .catch(() => {});
    },
  },
  showIds: {
    type: "button",
    title: "Show seen ids",
    onPress: () => {
      const hiddenIds = [...autoHidden.keys()];
      phoenix.ui
        .showSheet({
          title: "nonitroads ids",
          monospace: true,
          text:
            `Auto-hidden (${hiddenIds.length}):\n${hiddenIds.join("\n") || "none"}\n\n` +
            `Seen (${seen.size}, newest last):\n${[...seen].slice(-200).join("\n") || "none"}`,
        })
        .catch(() => {});
    },
  },
  resetStats: {
    type: "button",
    title: "Reset counters",
    onPress: () => {
      for (const key of ["offers", "quests", "views"]) phoenix.storage.set(`count.${key}`, 0);
      phoenix.ui.toast("nonitroads counters reset");
    },
  },
});

// ---- counters -------------------------------------------------------------------------------

const bump = (key: "offers" | "quests" | "views") => {
  const total = (phoenix.storage.get<number>(`count.${key}`) ?? 0) + 1;
  phoenix.storage.set(`count.${key}`, total);
  if (total === 1 || total % 25 === 0) phoenix.log.info(`${key}: ${total} blocked so far`);
};

const stats = () =>
  ["offers", "quests", "views"].map((k) => `${k}: ${phoenix.storage.get<number>(`count.${k}`) ?? 0}`).join(", ");

// ---- UI: hide by exact id -------------------------------------------------------------------

let hidden: Phoenix.Remover[] = [];

const applyHides = () => {
  for (const undo of hidden) undo();
  hidden = [];
  if (settings.hideGift) hidden.push(phoenix.ui.hide("composer.gift"));
  const ids = new Set(
    settings.extraIds
      .split("\n")
      .map((s) => s.trim())
      .filter(Boolean),
  );
  for (const id of ids) {
    try {
      hidden.push(phoenix.ui.hide(id));
    } catch (err) {
      phoenix.log.warn(`can't hide ${id}: ${err}`);
    }
  }
};

// ---- UI: hide by pattern --------------------------------------------------------------------

const seen = new Set<string>();
const autoHidden = new Map<string, Phoenix.Remover>();
let pattern: RegExp | null = null;

const compile = () => {
  const source = settings.autoPattern.trim();
  if (!source) return (pattern = null);
  try {
    return (pattern = new RegExp(source, "i"));
  } catch (err) {
    phoenix.log.warn(`bad auto-hide pattern: ${err}`);
    phoenix.ui.toast("nonitroads: the auto-hide pattern is not a valid regex");
    return (pattern = null);
  }
};

const consider = (id: string) => {
  if (autoHidden.has(id) || !settings.autoHide || !pattern || !pattern.test(id)) return;
  try {
    autoHidden.set(id, phoenix.ui.hide(id));
    bump("views");
    phoenix.log.info(`auto-hid ${id}`);
  } catch (err) {
    phoenix.log.warn(`can't auto-hide ${id}: ${err}`);
  }
};

// After a setting change: drop hides that no longer apply, then try every id seen so far.
const rescan = () => {
  compile();
  for (const [id, undo] of autoHidden) {
    if (!settings.autoHide || !pattern || !pattern.test(id)) {
      undo();
      autoHidden.delete(id);
    }
  }
  for (const id of seen) consider(id);
};

// Shows text to the user: a local chat reply if the host offers one, else a sheet, else a toast.
const say = (ctx: Phoenix.UiContext | undefined, text: string) => {
  try {
    if (typeof ctx?.reply === "function") return void ctx.reply(text);
  } catch (err) {
    phoenix.log.warn(`reply failed: ${err}`);
  }
  try {
    phoenix.ui.showSheet({ title: "nonitroads", text, monospace: true }).catch(() => {});
  } catch {
    phoenix.ui.toast(text.slice(0, 200));
  }
};

const report = (ids: boolean) => {
  if (ids) {
    const list = [...autoHidden.keys()];
    return `Auto-hidden ids (${list.length}):\n${list.join("\n") || "none"}`;
  }
  return (
    `nonitroads is running. Blocked ${stats()}. Seen ${seen.size} ids, ${autoHidden.size} auto-hidden.` +
    (loadErrors.length ? `\nLoad errors:\n${loadErrors.join("\n")}` : "")
  );
};

// ---- /nonitroads ----------------------------------------------------------------------------

safe("command", () => phoenix.commands.register({
  name: "nonitroads",
  description: "Shows what nonitroads blocked. Add `ids` to list hidden views.",
  execute(args, ctx) {
    say(ctx, report(args.trim() === "ids"));
  },
}));


// Fallback for builds where slash commands don't register: catch the typed text before it sends.
safe("onSend", () =>
  phoenix.messages.onSend((text, ctx) => {
    const t = text.trim();
    if (t !== "/nonitroads" && t !== "/nonitroads ids") return;
    try {
      say(ctx, report(t.endsWith("ids")));
    } catch (err) {
      phoenix.log.error(`/nonitroads failed: ${err instanceof Error ? err.stack ?? err.message : err}`);
    }
    return false;
  }),
);

safe("compile", compile);

// An empty prefix matches every node, so each new view reaches this handler once.
safe("onNode", () => phoenix.ui.onNode({ prefix: "" }, (node) => {
  const id = node.accessibilityId;
  if (!id) return;
  if (!seen.has(id)) {
    if (seen.size >= SEEN_LIMIT) seen.delete(seen.values().next().value as string);
    seen.add(id);
    if (settings.debug) phoenix.log.info(`ui id: ${id}`);
  }
  consider(id);
}));

safe("hide", applyHides);

// ---- API: path discovery --------------------------------------------------------------------

const seenPaths = new Set<string>();
safe("api log", () =>
  phoenix.discord.onRequest({}, (req) => {
    if (!settings.debug || seenPaths.size > 2000) return;
    const key = `${req.method} ${req.path.split("?")[0].replace(/\d{15,22}/g, ":id")}`;
    if (seenPaths.has(key)) return;
    seenPaths.add(key);
    phoenix.log.info(`api ${key}`);
  }),
);

// ---- API: empty the sources of the popups ---------------------------------------------------

type Rule = {
  includes: string;
  enabled: () => boolean;
  counter: "offers" | "quests";
  reply: () => Phoenix.ResponseHookResult;
};

const rules: Rule[] = [
  // Promo lists come back empty.
  { includes: "/outbound-promotions", enabled: () => settings.offers, counter: "offers", reply: () => ({ body: [] }) },
  { includes: "/promotions", enabled: () => settings.offers, counter: "offers", reply: () => ({ body: [] }) },
  // One trial or user offer has no empty form, so it reads as not found. The status change is
  // desktop only: mobile keeps the real status and gets an empty object.
  {
    includes: "/billing/user-offer",
    enabled: () => settings.offers,
    counter: "offers",
    reply: () => ({ status: 404, body: {} }),
  },
  {
    includes: "/billing/user-trial-offer",
    enabled: () => settings.offers,
    counter: "offers",
    reply: () => ({ status: 404, body: {} }),
  },
  // Quests are sponsored promos. An empty list means no banner, no popup, no badge.
  {
    includes: "/quests/@me",
    enabled: () => settings.quests,
    counter: "quests",
    reply: () => ({ body: { quests: [], excluded_quests: [], quest_enrollment_blocked_until: null } }),
  },
];

for (const rule of rules) {
  safe(`onResponse ${rule.includes}`, () =>
    phoenix.discord.onResponse({ method: "GET", pathIncludes: rule.includes }, (res) => {
      if (!rule.enabled() || res.status < 200 || res.status >= 300) return;
      bump(rule.counter);
      return rule.reply();
    }),
  );
}

phoenix.onUnload(() => {
  for (const undo of hidden) undo();
  for (const undo of autoHidden.values()) undo();
});

const loadedMsg = `nonitroads loaded${loadErrors.length ? ` with ${loadErrors.length} errors: ${loadErrors[0]}` : ""}`;
phoenix.log.info(loadedMsg);
try {
  phoenix.ui.toast(loadedMsg);
} catch {}
})();
