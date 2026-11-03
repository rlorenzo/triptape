<script lang="ts">
  // None of these controls has a backend command yet (no cache-clear,
  // face-data wipe, or persisted export settings), so they are rendered
  // disabled rather than reporting actions that never happened.
  const cloudPolish = false;
  const stripExif = true;
</script>

<div class="p-6 max-w-2xl mx-auto">
  <h1 class="text-2xl font-bold mb-4">Settings</h1>

  <div class="mb-4 p-3 rounded bg-amber-100 text-amber-900 text-sm">
    Preview — these settings are not wired up yet. Changing them has no effect.
  </div>

  <section class="border rounded p-4 mb-3 bg-white">
    <h2 class="font-semibold mb-1">Cache</h2>
    <p class="text-sm text-gray-600 mb-2">Thumbnails and proxies.</p>
    <button class="border rounded px-3 py-1 text-sm disabled:opacity-50" disabled>Clear cache</button>
  </section>

  <section class="border rounded p-4 mb-3 bg-white">
    <h2 class="font-semibold mb-1">Cloud Polish</h2>
    <label class="text-sm flex gap-2 items-center opacity-60">
      <input type="checkbox" checked={cloudPolish} disabled />
      Include photos in Cloud Polish for new trips (off by default)
    </label>
    <label class="text-sm flex gap-2 items-center mt-2 opacity-60">
      <input type="checkbox" checked={stripExif} disabled />
      Strip EXIF on export (location, camera serial)
    </label>
  </section>

  <section class="border rounded p-4 mb-3 bg-white">
    <h2 class="font-semibold mb-1">Face data</h2>
    <p class="text-sm text-gray-600 mb-2">Faces are detected on this computer. Names never leave the device.</p>
    <button class="border border-red-400 text-red-700 rounded px-3 py-1 text-sm disabled:opacity-50" disabled>Delete all face data</button>
  </section>

  <section class="border rounded p-4 bg-white">
    <h2 class="font-semibold mb-1">About / Licenses</h2>
    <p class="text-sm text-gray-600">TripTape 0.1.0 — MIT. FFmpeg is a separate GPL sidecar executable; see docs/ffmpeg-build.md and the source link in About. Bundled fonts: Inter / Source Serif (OFL).</p>
  </section>
</div>
