---
paths:
    - "pi/extensions/**"
    - "pi/workflows/extensions/**"
---

# Tool schemas

Every tool the pi package registers must work with every model, not only GPT. Claude Code's bridge drops `anyOf`,
`oneOf`, and `allOf` at the root of a tool's parameters, so a tool whose input is a union reaches Claude as an empty
object; GLM rejects such tools outright (#81).

- **One tool per operation.** Give each operation its own tool with its own name (`create_worker`, `delete_worker`),
  never one tool whose payload picks the operation.
- **An object at the root.** `parameters` is a `Type.Object`; never a union, intersection, or `Type.Ref` at the top.
- **String enums for fixed choices.** A fixed choice is `Type.Enum([...], { type: "string" })`, never a `const` or a
  union of literals.
- **No optional-field dispatch.** Don't fold different operations into one tool through optional fields whose
  validity depends on each other. Two operations sharing a shape is no reason to merge them.
- **Unions inside fields are fine** when they're data, not operations (a Project specification, a registration
  batch): the bridge keeps nested unions.
- **Names** are unique, ASCII `[A-Za-z0-9_-]`, at most 50 characters.

`pi/tests/tool-schemas.test.ts` checks every registered tool against these rules and against the bridge's
normalization (`pi/tests/claude-code-schema-fixture.ts`). A new extension's tools must be in its registry.
