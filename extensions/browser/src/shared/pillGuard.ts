interface PillGuardRegistry {
  roots: WeakMap<EventTarget, ShadowRoot>;
  replayed: WeakSet<Event>;
  pointers: WeakMap<ShadowRoot, Map<number, Element>>;
  pressed: WeakMap<ShadowRoot, { id: number; button: Element }>;
  installed: boolean;
}

// The two IIFE bundles share this registry in the extension's isolated world.
const key = Symbol.for("slipstream.pillGuard");
const world = globalThis as typeof globalThis & { [key]?: PillGuardRegistry };
const registry: PillGuardRegistry = world[key] ?? {
  roots: new WeakMap(),
  replayed: new WeakSet(),
  pointers: new WeakMap(),
  pressed: new WeakMap(),
  installed: false,
};
if (!world[key]) Object.defineProperty(world, key, { value: registry });

export function registerPillHost(host: HTMLElement, root: ShadowRoot): void {
  registry.roots.set(host, root);
}

export function unregisterPillHost(host: HTMLElement): void {
  registry.roots.delete(host);
}

export function setPillPointer(host: HTMLElement, pointerId: number, target?: Element): void {
  const root = registry.roots.get(host);
  if (!root) return;
  let pointers = registry.pointers.get(root);
  if (!target) {
    pointers?.delete(pointerId);
    return;
  }
  if (!pointers) registry.pointers.set(root, pointers = new Map());
  pointers.set(pointerId, target);
}

export function isPillHost(target: EventTarget): boolean {
  return registry.roots.has(target);
}

export function isReplayedUserEvent(event: Event): boolean {
  return registry.replayed.has(event);
}

export function routePillEvent(event: MouseEvent): void {
  if (isReplayedUserEvent(event)) return;
  const root = (event.target && registry.roots.get(event.target))
    || event.composedPath?.().map((target) => registry.roots.get(target)).find(Boolean);
  if (!root) return;

  event.stopImmediatePropagation();
  if (!event.isTrusted) return;

  const pointer = event.type.startsWith("pointer") ? event as PointerEvent : undefined;
  let inner: Element | null | undefined;
  if (pointer?.type === "pointerup") {
    const pointers = registry.pointers.get(root);
    inner = pointers?.get(pointer.pointerId);
    pointers?.delete(pointer.pointerId);
  }
  if (!inner && event.type === "click" && event.detail === 0) inner = root.activeElement;
  inner ??= root.elementFromPoint?.(event.clientX, event.clientY);
  if (pointer?.type === "pointerdown" && event.button === 0) {
    const button = inner?.closest("button");
    if (button) registry.pressed.set(root, { id: pointer.pointerId, button });
    else registry.pressed.delete(root);
  } else if (pointer?.type === "pointercancel" && registry.pressed.get(root)?.id === pointer.pointerId) {
    registry.pressed.delete(root);
  } else if (event.type === "click" && event.detail > 0) {
    const pressed = registry.pressed.get(root);
    registry.pressed.delete(root);
    if (!pressed || inner?.closest("button") !== pressed.button) return;
    inner = pressed.button;
  }
  if (!inner || inner.getRootNode() !== root) return;

  const init: PointerEventInit = {
    bubbles: true,
    cancelable: event.cancelable,
    composed: false,
    clientX: event.clientX, clientY: event.clientY,
    screenX: event.screenX, screenY: event.screenY,
    button: event.button, buttons: event.buttons, detail: event.detail,
    ctrlKey: event.ctrlKey, shiftKey: event.shiftKey, altKey: event.altKey, metaKey: event.metaKey,
  };
  const view = root.ownerDocument.defaultView;
  const replay = pointer && view?.PointerEvent ? new view.PointerEvent(event.type, {
    ...init,
    pointerId: pointer.pointerId, pointerType: pointer.pointerType, isPrimary: pointer.isPrimary,
    width: pointer.width, height: pointer.height, pressure: pointer.pressure,
    tangentialPressure: pointer.tangentialPressure, tiltX: pointer.tiltX, tiltY: pointer.tiltY, twist: pointer.twist,
  }) : new (view?.MouseEvent ?? MouseEvent)(event.type, init);
  registry.replayed.add(replay);
  if (!inner.dispatchEvent(replay)) event.preventDefault();
}

export function installPillGuard(window: Window): void {
  if (registry.installed) return;
  registry.installed = true;
  // Window capture runs before document and player capture listeners.
  for (const type of ["pointerdown", "pointerup", "pointercancel", "mousedown", "mouseup", "click", "dblclick", "auxclick", "contextmenu"]) {
    window.addEventListener(type, (event) => routePillEvent(event as MouseEvent), true);
  }
}
