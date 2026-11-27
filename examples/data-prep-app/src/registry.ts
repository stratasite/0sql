// Who may do what with which dataset: this app's records, not zsqld's.
//
// zsqld keeps no user. A dataset is its project's and is addressed by its
// id, so the access control is here, and every route checks it before
// anything reaches zsqld:
//
// - the user who imported a dataset owns it: they may read, change,
//   share and delete it;
// - a user it is shared with may read it, and use it in a blend, and
//   nothing more.
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

  /** The datasets `user` may see, with what they may do. */
  visible(user: string): Map<string, Access> {
    const out = new Map<string, Access>();
    for (const id of this.#entries.keys()) {
      const a = this.access(user, id);
      if (a) out.set(id, a);
    }
    return out;
  }

  add(id: string, owner: string): void {
    this.#entries.set(id, { owner, sharedWith: [], created: new Date().toISOString() });
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
