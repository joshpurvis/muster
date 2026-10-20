import { anchoredScroll, clampZoom, fitScale, stepZoom, svgFromDataUri, svgSize, zoomLimits } from "./zoom.ts";
import { check, eq } from "./test_util.ts";

Deno.test("fit: raster pictures shrink to the box but are not enlarged; sizeless vectors fill it", () => {
  eq(fitScale(2000, 1000, 500, 500), 0.25);
  eq(fitScale(100, 100, 500, 500), 1);
  eq(fitScale(100, 100, 500, 400, true), 4);
  eq(fitScale(0, 0, 500, 500), 1);
});

Deno.test("zoom steps go from the scale on screen, and Fit is a stop on the way", () => {
  eq(stepZoom(0.43, 1, 0.43), 0.5, "in from a fitted 43%");
  eq(stepZoom(0.43, -1, 0.43), 0.33, "out from a fitted 43%");
  eq(stepZoom(1, 1, 1), 1.25);
  eq(stepZoom(1, -1, 1), 0.9);
  eq(stepZoom(2, -1, 0.43), 1.5);
  eq(stepZoom(0.5, -1, 0.43), 0.43, "zooming out stops at Fit");
  eq(stepZoom(16, 1, 0.5), 16, "the largest step holds");
  eq(stepZoom(0.05, -1, 0.5), 0.05, "the smallest step holds");
  eq(stepZoom(0.02, 1, 0.02), 0.05, "a huge picture fitted below every step can still zoom in");
  eq(stepZoom(0.05, -1, 0.02), 0.02, "...and back out to its fit");
  eq(zoomLimits(0.02), [0.02, 16]);
  eq(zoomLimits(30), [0.05, 30], "a tiny vector fitted past every step");
  eq(clampZoom(100, 1), 16);
  eq(clampZoom(0.001, 1), 0.05);
});

Deno.test("zooming keeps the point under the pointer still", () => {
  // A 1000 px wide picture at 50% (500 px) centred in an 800 px stage: offset 150, no scroll.
  // The pointer at 400 (the picture's middle, natural x 500). At 200% it is 2000 px, offset 0.
  eq(anchoredScroll(0, 400, 150, 0, 0.5, 2), 600);
  check(Math.abs((600 + 400 - 0) / 2 - 500) < 1e-9, "natural x 500 is still under the pointer");
  // Already scrolled: at scale 1, scroll 300, pointer 100 → natural 400 (offset 0). At 2: 400*2-100.
  eq(anchoredScroll(300, 100, 0, 0, 1, 2), 700);
  eq(anchoredScroll(0, 0, 0, 0, 1, 0.5), 0, "never a negative scroll");
});

Deno.test("svg sizes: absolute width and height, one side and a viewBox, or only a viewBox", () => {
  eq(svgSize(`<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100">`), { w: 200, h: 100, sized: true });
  eq(svgSize(`<?xml version="1.0"?><svg width='30pt' height="15pt" viewBox="0 0 40 20">`), { w: 40, h: 20, sized: true });
  eq(svgSize(`<svg width="400px" viewBox="0 0 400 200">`), { w: 400, h: 200, sized: true });
  eq(svgSize(`<svg viewBox="0 0 400 200">`), { w: 400, h: 200, sized: false });
  eq(svgSize(`<svg width="100%" viewBox="0,0,400,100" style="max-width: 400px">`), { w: 400, h: 100, sized: false });
  eq(svgSize(`<svg width="100%">`), null);
  eq(svgSize(`not an svg`), null);
});

Deno.test("svg text from a data URI, base64 or percent-encoded", () => {
  const text = `<svg viewBox="0 0 4 2"></svg>`;
  eq(svgFromDataUri(`data:image/svg+xml;base64,${btoa(text)}`), text);
  eq(svgFromDataUri(`data:image/svg+xml,${encodeURIComponent(text)}`), text);
  eq(svgFromDataUri(`data:image/png;base64,iVBORw0KGgo=`), null);
  eq(svgFromDataUri(`data:image/svg+xml;base64,@@@`), null);
});
