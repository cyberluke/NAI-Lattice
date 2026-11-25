import { onBeforeUnmount, shallowRef, type ShallowRef } from 'vue';

/** A one-second clock with a lifecycle owner; no global interval leaks. */
export function useNow(intervalMs = 1000): ShallowRef<number> {
  const now = shallowRef(Date.now());
  const timer = setInterval(() => {
    now.value = Date.now();
  }, intervalMs);
  onBeforeUnmount(() => clearInterval(timer));
  return now;
}
