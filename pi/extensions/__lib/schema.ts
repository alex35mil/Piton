/**
 * Validation with pi-schema, the validator pi itself uses for tool arguments.
 *
 * Schemas stay TypeBox builders: pi still types extension tools with TypeBox, and pi-schema's
 * `compile()` reads TypeBox schemas as JSON Schema and keeps their static types. Compile each
 * schema once where it's defined, then validate through the compiled validator.
 */
import {
    compile as compileSchema,
    type JsonSchema,
    type Static,
    type ValidationError,
    type Validator,
} from "@earendil-works/pi-schema"

export type { Validator } from "@earendil-works/pi-schema"

// The schema each validator was compiled from, so errors can name what the schema accepts.
const schemas = new WeakMap<Validator<unknown>, JsonSchema>()

/** pi-schema's `compile()`, remembering the schema so {@link parse} errors can list accepted fields. */
export function compile<S extends JsonSchema>(schema: S): Validator<Static<S>> {
    const validator = compileSchema(schema)
    schemas.set(validator, schema)
    return validator
}

/** One line per error: the JSON Pointer of the failing value (`/` for the value itself) and pi-schema's message. */
export function describeErrors(errors: readonly ValidationError[]): string[] {
    return errors.map((error) => `${error.path || "/"}: ${error.message}`)
}

/**
 * The property names a schema accepts at a JSON Pointer, across every union branch. Empty when the
 * pointer leads to no object schema.
 */
export function acceptedFields(schema: JsonSchema, pointer: string): string[] {
    const segments = pointer
        .split("/")
        .slice(1)
        .map((segment) => segment.replaceAll("~1", "/").replaceAll("~0", "~"))
    let current = branches(schema)
    for (const segment of segments)
        current = current.flatMap((node) => {
            const properties = record(node.properties)
            if (properties && segment in properties) return branches(properties[segment] as JsonSchema)
            if (node.items !== undefined && /^\d+$/.test(segment)) return branches(node.items as JsonSchema)
            return []
        })
    return [...new Set(current.flatMap((node) => Object.keys(record(node.properties) ?? {})))]
}

function branches(schema: JsonSchema): Record<string, unknown>[] {
    const node = record(schema)
    if (!node) return []
    const union = [node.anyOf, node.oneOf, node.allOf].find(Array.isArray) as JsonSchema[] | undefined
    return union ? union.flatMap(branches) : [node]
}

function record(value: unknown): Record<string, unknown> | undefined {
    return value !== null && typeof value === "object" && !Array.isArray(value)
        ? (value as Record<string, unknown>)
        : undefined
}

/**
 * Raised by {@link parse}: the formatted errors in the message, pi-schema's errors as `cause`. An unknown
 * field's line also lists the fields accepted where it appeared, when the schema is known.
 */
export class SchemaValidationError extends Error {
    override readonly cause: readonly ValidationError[]

    constructor(errors: readonly ValidationError[], schema?: JsonSchema) {
        const lines = errors.map((error) => {
            const line = `${error.path || "/"}: ${error.message}`
            if (schema === undefined || error.message !== "unknown field") return line
            const parent = error.path.slice(0, error.path.lastIndexOf("/"))
            const accepted = acceptedFields(schema, parent)
            return accepted.length ? `${line} (accepted: ${accepted.join(", ")})` : line
        })
        super(`invalid input: ${[...new Set(lines)].join("; ")}`)
        this.name = "SchemaValidationError"
        this.cause = errors
    }
}

/**
 * `value` itself when the validator accepts it, else a {@link SchemaValidationError}. Strict: nothing is
 * stripped, converted, or defaulted.
 */
export function parse<T>(validator: Validator<T>, value: unknown): T {
    const result = validator.validate(value)
    if (!result.ok) throw new SchemaValidationError(result.errors, schemas.get(validator as Validator<unknown>))
    return result.value
}
