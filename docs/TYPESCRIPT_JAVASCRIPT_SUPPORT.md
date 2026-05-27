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
- imports, re-exports, and CommonJS `require(...)`
- functions and arrow-function exports
- classes
- interfaces
- enums
- type aliases
- test symbols from `.test.*`, `.spec.*`, and `__tests__` paths
- AWS CDK app commands from `cdk.json`
- AWS CDK stack classes
- AWS CDK construct/resource declarations such as Lambda functions, DynamoDB tables, queues, buckets, and API resources

## Current Limits

The extractor is syntax-aware by source patterns, not a full TypeScript compiler frontend. It does not run `tsc`, resolve path aliases, infer types, or evaluate framework conventions yet.

Future adapters should keep the same persisted graph contract and improve resolution with TypeScript compiler metadata, framework detectors, deeper AWS CDK construct/property extraction, and Kubernetes manifest extraction.
