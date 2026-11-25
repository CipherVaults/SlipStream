import { extension as api } from "./shared/browser";
import { installPillGuard } from "./shared/pillGuard";

installPillGuard(window);
void api.runtime.sendMessage({ kind: "frame-reset" }).catch(() => {});
