// Each dataset's pipeline: this app's, not zsqld's.
//
// zsqld runs a graph it is sent and keeps a copy with what it made; it
// keeps no pipeline of its own. So the graph a user is editing, every
// version of it, the edits they made in the grid, and when it runs are
// the application's. This keeps them in a JSON file for the demo; your
// app keeps them in its own database, where it can diff versions, keep
// drafts, and say who changed what.

import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import type { Graph } from "./zsql.js";

export interface Version {
  version: number;
  graph: Graph;
  saved: string;
  by: string;
  /** The run that made the dataset's output from it. */
  run?: string;
}

export class Pipelines {
  #all: Map<string, Version[]>;

  constructor(private readonly path: string) {
    try {
      this.#all = new Map(Object.entries(JSON.parse(readFileSync(path, "utf8")) as Record<string, Version[]>));
    } catch {
      this.#all = new Map();
    }
  }

  /** A dataset's latest version, if it has one. */
  latest(dataset: string): Version | undefined {
    return this.#all.get(dataset)?.at(-1);
  }

  versions(dataset: string): Version[] {
    return this.#all.get(dataset) ?? [];
  }

  /** A new version, made against the one the user was looking at. */
  save(dataset: string, graph: Graph, by: string, run?: string): Version {
    const list = this.#all.get(dataset) ?? [];
    const v: Version = { version: (list.at(-1)?.version ?? 0) + 1, graph, saved: new Date().toISOString(), by, run };
    list.push(v);
    this.#all.set(dataset, list);
    this.#write();
    return v;
  }

  /** The datasets whose latest graph reads a source or a dataset. */
  readers(id: string): string[] {
    return [...this.#all.entries()]
      .filter(([, list]) => list.at(-1)?.graph.nodes.some((n) => ("source" in n && n.source === id) || ("dataset" in n && n.dataset === id)))
      .map(([d]) => d);
  }

  remove(dataset: string): void {
    this.#all.delete(dataset);
    this.#write();
  }

  #write(): void {
    mkdirSync(dirname(this.path), { recursive: true });
    const tmp = `${this.path}.tmp-${process.pid}`;
    writeFileSync(tmp, JSON.stringify(Object.fromEntries(this.#all), null, 2));
    renameSync(tmp, this.path);
  }
}
