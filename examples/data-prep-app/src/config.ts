// Everything the app is told, from the environment.

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
  /** Signs the session cookie. Set it to something secret outside a demo. */
  sessionSecret: string;
  /** Where this app keeps its own records: who owns and may read which dataset. */
  dataDir: string;
  /** The largest file a user may upload; zsqld has its own cap too. */
  maxUploadBytes: number;
}

export function fromEnv(env: NodeJS.ProcessEnv = process.env): Config {
  return {
    port: Number(env.PORT ?? 3700),
    zsqldUrl: env.ZSQLD_URL ?? "http://127.0.0.1:3699",
    org: env.ZSQL_ORG ?? "default",
    project: env.ZSQL_PROJECT ?? "grocery-store",
    branch: env.ZSQL_BRANCH ?? "main",
    sessionSecret: env.SESSION_SECRET ?? "change me: a demo secret, not a real one",
    dataDir: env.DATA_DIR ?? "data",
    maxUploadBytes: Number(env.MAX_UPLOAD_BYTES ?? 64 * 1024 * 1024),
  };
}
