// The fullscreen switch at the right end of a panel's strip and of the web preview's bar.
import { toggleFullscreen } from "../actions.ts";
import { IconMaximize, IconMinimize } from "../icons.tsx";

export function FullscreenButton({ tab, full }: { tab: number; full: boolean }) {
  const label = full ? "Exit fullscreen" : "Fullscreen";
  return (
    <button
      type="button"
      class={full ? "ps-full on" : "ps-full"}
      data-fullscreen-toggle=""
      aria-label={label}
      aria-pressed={full}
      data-tip={label}
      data-keys={full ? "Esc" : "Alt+Shift+Z"}
      onPointerDown={(e) => e.stopPropagation()}
      onClick={(e) => {
        toggleFullscreen(tab);
        // The button stays where it is, so Esc (or Enter again) goes back from here.
        (e.currentTarget as HTMLElement).focus();
      }}
    >
      {full ? <IconMinimize size={13} /> : <IconMaximize size={13} />}
    </button>
  );
}
