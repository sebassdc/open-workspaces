// Node strips types but, loading index.ts as an ES module, needs file extensions
// that ts-node (which runs the component under Pulumi) does not want: retry
// a missing relative import with .ts.
import { registerHooks } from "node:module";

registerHooks({
    resolve(specifier, context, next) {
        try {
            return next(specifier, context);
        } catch (error) {
            if (error?.code !== "ERR_MODULE_NOT_FOUND" || !specifier.startsWith(".")) throw error;
            return next(`${specifier}.ts`, context);
        }
    },
});
