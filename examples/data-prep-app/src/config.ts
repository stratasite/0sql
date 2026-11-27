// Everything the app is told, from the environment.

import { randomBytes } from "node:crypto";

export interface Config {
  /** This app's own port. */
  port: number;
  /** Where zsqld is. Your backend reaches it; browsers never do. */
  zsqldUrl: string;
  /** The org and project the datasets belong to. The project must be deployed. */
  org: string;
  project: string;
  /**
   * The branch a blend plans the model's part on. Datasets themselves
   * belong to the project, not to a branch: this is for blends only.
   */
  branch: string;
  /** Signs the session cookie. Unset, a random one for the process. */
  sessionSecret: string;
  /** Where this app keeps its own records: who owns and may read which dataset. */
  dataDir: string;
  /** The largest file a user may upload; zsqld has its own cap too. */
  maxUploadBytes: number;
}

export function fromEnv(env: NodeJS.ProcessEnv = process.env): Config {
  // With no secret set, one made for this process: sessions end when it
  // stops, and no cookie can be signed by anyone who read this file.
  const sessionSecret = env.SESSION_SECRET ?? randomBytes(32).toString("hex");
  if (!env.SESSION_SECRET) console.warn("SESSION_SECRET is not set: sessions last until this process stops");
  return {
    port: Number(env.PORT ?? 3700),
    zsqldUrl: env.ZSQLD_URL ?? "http://127.0.0.1:3699",
    org: env.ZSQL_ORG ?? "default",
    project: env.ZSQL_PROJECT ?? "grocery-store",
    branch: env.ZSQL_BRANCH ?? "main",
    sessionSecret,
    dataDir: env.DATA_DIR ?? "data",
    maxUploadBytes: Number(env.MAX_UPLOAD_BYTES ?? 64 * 1024 * 1024),
  };
}
