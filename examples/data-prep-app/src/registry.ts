// Who may do what with which dataset and source: this app's records, not
// zsqld's.
//
// zsqld keeps no user. A dataset or a source is its project's and is
// addressed by its id, so the access control is here, and every route
// checks it before anything reaches zsqld:
//
// - the user who imported a file owns its source and the dataset over
//   it: they may read, change, share and delete them, and use the
//   source in another dataset's pipeline;
// - a user a dataset is shared with may read it, use it in a blend, and
//   build on it in a pipeline of their own, and nothing more. Sharing a
//   dataset shares its rows, not the sources it was made from.
//
// Kept in a JSON file for the demo. Your app keeps this in its own
// database, beside whatever else it shares (charts, dashboards), and may
// grant reading by any rule it likes: a team, a link, a shared chart.

import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

export type Access = "owner" | "reader";

export interface Entry {
  owner: string;
  sharedWith: string[];
  created: string;
  /** A source, or a dataset (the default). */
  kind?: "source" | "dataset";
  /** For a dataset over a file: the source a new version of the file goes to. */
  source?: string;
}

export class Registry {
  // A Map, not an object: an id is the user's to send, and `__proto__` is
  // no dataset.
  #entries: Map<string, Entry>;

  constructor(private readonly path: string) {
    try {
      this.#entries = new Map(Object.entries(JSON.parse(readFileSync(path, "utf8")) as Record<string, Entry>));
    } catch {
      this.#entries = new Map();
    }
  }

  /** What `user` may do with dataset `id`; `undefined` is nothing at all. */
  access(user: string, id: string): Access | undefined {
    const e = this.#entries.get(id);
    if (!e) return undefined;
    if (e.owner === user) return "owner";
    return e.sharedWith.includes(user) ? "reader" : undefined;
  }

  entry(id: string): Entry | undefined {
    return this.#entries.get(id);
  }

  /** The datasets, or the sources, `user` may see, with what they may do. */
  visible(user: string, kind: "source" | "dataset" = "dataset"): Map<string, Access> {
    const out = new Map<string, Access>();
    for (const [id, e] of this.#entries) {
      const a = this.access(user, id);
      if (a && (e.kind ?? "dataset") === kind) out.set(id, a);
    }
    return out;
  }

  add(id: string, owner: string, more: Pick<Entry, "kind" | "source"> = {}): void {
    this.#entries.set(id, { owner, sharedWith: [], created: new Date().toISOString(), ...more });
    this.#save();
  }

  share(id: string, user: string): void {
    const e = this.#entries.get(id);
    if (e && user !== e.owner && !e.sharedWith.includes(user)) {
      e.sharedWith.push(user);
      this.#save();
    }
  }

  unshare(id: string, user: string): void {
    const e = this.#entries.get(id);
    if (e) {
      e.sharedWith = e.sharedWith.filter((u) => u !== user);
      this.#save();
    }
  }

  remove(id: string): void {
    this.#entries.delete(id);
    this.#save();
  }

  #save(): void {
    mkdirSync(dirname(this.path), { recursive: true });
    const tmp = `${this.path}.tmp-${process.pid}`;
    writeFileSync(tmp, JSON.stringify(Object.fromEntries(this.#entries), null, 2));
    renameSync(tmp, this.path);
  }
}
