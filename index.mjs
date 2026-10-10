import { createRequire } from "module";
const require = createRequire(import.meta.url);
const susee_bundler = require("./index.cjs");

export const analyze = susee_bundler.analyze;
export const graph = susee_bundler.graph;
export const LogTimer = susee_bundler.LogTimer;
export const logError = susee_bundler.logError;
export const logInfo = susee_bundler.logInfo;
export const logWarning = susee_bundler.logWarning;
export const suseeBundler = susee_bundler.suseeBundler;
export const suseeLint = susee_bundler.suseeLint;
