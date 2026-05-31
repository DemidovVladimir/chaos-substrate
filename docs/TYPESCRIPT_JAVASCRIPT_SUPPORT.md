# TypeScript and JavaScript Support

Chaos Substrate supports TypeScript and JavaScript repositories through Rust code only.

## Captured Inputs

- `package.json`
- `tsconfig.json`
- `jsconfig.json`
- `.ts`, `.tsx`, `.mts`, `.cts`, `.d.ts`
- `.js`, `.jsx`, `.mjs`, `.cjs`

## Extracted Knowledge

- npm dependencies from `dependencies`, `devDependencies`, `peerDependencies`, and `optionalDependencies`
- npm scripts
- ES module `import` declarations (as dependency edges)
- functions, arrow/const-function exports, and class methods
- classes
- interfaces
- enums
- type aliases
- test symbols from `.test.*`, `.spec.*`, `__tests__`, and `__test__` paths
- AWS CDK app commands from `cdk.json`
- AWS CDK stack classes
- AWS CDK construct/resource declarations such as Lambda functions, DynamoDB tables, queues, buckets, and API resources

## How Extraction Works

Extraction is performed with the [oxc](https://github.com/oxc-project/oxc) AST parser, a real TypeScript/JavaScript parser. It builds a full syntax tree rather than matching source patterns, so it captures class methods, arrow/const-function exports, and accurate line spans for every symbol.

## Current Limits

The parser is a syntactic frontend, not a full TypeScript compiler. It does not run `tsc`, resolve path aliases, or infer types, and it does not evaluate framework conventions yet. Cross-file call resolution is name-based.

Future adapters should keep the same persisted graph contract and improve resolution with TypeScript compiler metadata, framework detectors, deeper AWS CDK construct/property extraction, and Kubernetes manifest extraction.
