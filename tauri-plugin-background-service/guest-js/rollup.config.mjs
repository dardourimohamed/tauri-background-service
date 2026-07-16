import typescript from '@rollup/plugin-typescript';

// Rollup receives TypeScript's source path relative to the plugin package
// root, then rebases it once more for `dist-js`, producing `../../index.ts`
// (outside `guest-js`) in the generated maps. This package has one entrypoint;
// pin the map to the real source beside `dist-js` so consumers and test runners
// can resolve it without warnings.
const sourcemapPathTransform = () => '../index.ts';

export default {
  input: 'index.ts',
  output: [
    {
      file: 'dist-js/index.js',
      format: 'esm',
      sourcemap: true,
      sourcemapPathTransform,
    },
    {
      file: 'dist-js/index.cjs',
      format: 'cjs',
      sourcemap: true,
      sourcemapPathTransform,
    },
  ],
  external: /^@tauri-apps\/api/,
  plugins: [typescript()],
};
