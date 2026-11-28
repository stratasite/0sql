// Starts the app: `npm start`, with zsqld running and the project deployed.

import { buildApp } from "./app.js";
import { fromEnv } from "./config.js";

const config = fromEnv();
const app = await buildApp(config);
await app.listen({ port: config.port, host: "127.0.0.1" });
app.log.info(`data prep demo on http://127.0.0.1:${config.port}, datasets in ${config.org}/${config.project} on ${config.zsqldUrl}`);
