/** Notes can survive a reload in browser storage even before the daemon saves them. */
export function leaveWithDrafts(
  unsaved: number[],
  persist: () => boolean,
  go: () => void,
  prompt: (ids: number[]) => void,
) {
  if (!unsaved.length || persist()) go();
  else prompt(unsaved);
}
