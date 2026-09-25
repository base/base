#!/usr/bin/env node

const fs = require("fs");
const path = require("path");
const { get_encoding } = require("tiktoken");

const DEFAULT_LIMIT = 8_000;
const DEFAULT_PATH = "docs/FEATURE_MAP.md";
const ENCODING = "o200k_base";

function parseArguments(arguments_) {
  const options = { limit: DEFAULT_LIMIT, path: DEFAULT_PATH };

  for (let index = 0; index < arguments_.length; index += 1) {
    const argument = arguments_[index];
    if (argument === "--limit") {
      options.limit = Number(arguments_[index + 1]);
      index += 1;
    } else if (argument === "--path") {
      options.path = arguments_[index + 1];
      index += 1;
    } else {
      throw new Error(`unknown argument: ${argument}`);
    }
  }

  if (!Number.isSafeInteger(options.limit) || options.limit < 1) {
    throw new Error("--limit must be a positive integer");
  }
  if (!options.path) {
    throw new Error("--path requires a value");
  }

  return options;
}

function appendSummary(message) {
  if (process.env.GITHUB_STEP_SUMMARY) {
    fs.appendFileSync(process.env.GITHUB_STEP_SUMMARY, `${message}\n`);
  }
}

function main() {
  const options = parseArguments(process.argv.slice(2));
  const document = fs.readFileSync(path.resolve(options.path), "utf8");
  const encoding = get_encoding(ENCODING);
  const tokenCount = encoding.encode(document).length;
  encoding.free();

  const result = `FEATURE_MAP.md: ${tokenCount.toLocaleString("en-US")} ${ENCODING} tokens (limit: fewer than ${options.limit.toLocaleString("en-US")})`;
  console.log(result);
  appendSummary(`## Feature map token budget\n\n${result}`);

  if (tokenCount >= options.limit) {
    throw new Error(
      `${options.path} is ${tokenCount.toLocaleString("en-US")} tokens; it must stay below ${options.limit.toLocaleString("en-US")} ${ENCODING} tokens.`,
    );
  }
}

try {
  main();
} catch (error) {
  console.error(`ERROR: ${error.message}`);
  process.exitCode = 1;
}
