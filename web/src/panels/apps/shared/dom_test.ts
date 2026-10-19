import { cycle } from "./dom.ts";
import { eq } from "./test_util.ts";

Deno.test("focus trap: Tab and Shift+Tab wrap inside the modal view", () => {
  eq([cycle(3, 0, false), cycle(3, 2, false), cycle(3, 0, true), cycle(3, -1, false), cycle(3, -1, true), cycle(0, 0, false)], [1, 0, 2, 0, 2, -1]);
});
