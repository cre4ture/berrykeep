/**
 * MapLibre requires WebGL. Explicit SwiftShader keeps browser tests portable on
 * headless runners whose default GPU configuration disables WebGL.
 */
export const playwrightWebGlLaunchOptions = {
  args: ["--use-gl=angle", "--use-angle=swiftshader", "--enable-unsafe-swiftshader"]
};
