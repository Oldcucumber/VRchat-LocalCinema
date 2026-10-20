import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { test } from 'node:test'
import { createRequire } from 'node:module'
import ts from 'typescript'
import * as vue from 'vue'
import { parse } from '@vue/compiler-sfc'

const require = createRequire(import.meta.url)
const { descriptor } = parse(readFileSync(new URL('../src/pages/SettingsPage.vue', import.meta.url), 'utf8'))
const { outputText } = ts.transpileModule(descriptor.scriptSetup.content, {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
})

const clone = value => JSON.parse(JSON.stringify(value))

async function settingsHarness() {
  const mounted = []
  const unmounted = []
  const timers = new Map()
  const calls = []
  const errors = []
  let now = 0
  let timerId = 0
  let holdNextSave = false
  let releaseSave
  let initial
  const store = {
    settingsCache: vue.ref(null),
    mediaStore: vue.reactive({ items: [] }),
    largeMediaItems: vue.ref([]),
  }
  const api = {
    getSettings: async () => clone(initial),
    getNetworkInterfaces: async () => [],
    getAppPaths: async () => null,
    detectPlayers: async () => [],
    getComponentsStatus: async () => [],
    getConvertCacheInfo: async () => null,
    onComponentDownloadProgress: async () => () => {},
    saveSettings: async settings => {
      calls.push(clone(settings))
      if (holdNextSave) {
        holdNextSave = false
        await new Promise(resolve => { releaseSave = resolve })
      }
    },
  }
  const window = {
    setTimeout(callback, delay) {
      const id = ++timerId
      timers.set(id, { callback, at: now + delay })
      return id
    },
    clearTimeout(id) { timers.delete(id) },
  }
  const mockRequire = id => {
    if (id === 'vue') return { ...vue, onMounted: fn => mounted.push(fn), onUnmounted: fn => unmounted.push(fn) }
    if (id === '../lib/api') return api
    if (id === '../lib/store') return store
    if (id === '../lib/toast') return { toast: { error: error => errors.push(error), info() {}, success() {} }, errMsg: String }
    if (id === '../lib/confirm') return { confirmDialog: async () => true }
    if (id === '../lib/format') return { formatSize: String }
    if (id === '@tauri-apps/plugin-dialog') return { open: async () => null }
    if (id.endsWith('.vue') || id === 'lucide-vue-next') return {}
    return require(id)
  }
  const scope = vue.effectScope()
  const page = scope.run(() => new Function('require', 'exports', 'window', `${outputText}\nreturn { form, persist, ready, saving, portInvalid, setBitrate }`)(mockRequire, {}, window))
  initial = clone(page.form)
  store.settingsCache.value = clone(initial)
  await vue.nextTick()
  for (const fn of mounted) await fn()
  await vue.nextTick()

  async function settle() {
    for (let i = 0; i < 8; i++) await vue.nextTick()
  }
  return {
    ...page, store, calls, errors,
    async advance(ms) {
      await settle()
      const end = now + ms
      for (;;) {
        const next = [...timers].filter(([, timer]) => timer.at <= end).sort((a, b) => a[1].at - b[1].at)[0]
        if (!next) break
        now = next[1].at
        timers.delete(next[0])
        next[1].callback()
        await settle()
      }
      now = end
      await settle()
    },
    holdSave() { holdNextSave = true },
    async releaseSave() { releaseSave(); await settle() },
    dispose() { scope.stop(); for (const fn of unmounted) fn() },
  }
}

test('new compression settings use upstream automatic saving and preserve close settings', async () => {
  const h = await settingsHarness()
  try {
    await h.advance(1000)
    assert.equal(h.calls.length, 0, 'loading must not save placeholder settings')
    h.form.streamingAv1Preset = 6
    h.form.streamingAutoResolution = true
    h.form.compatiblePreset = 'medium'
    h.setBitrate('compatible720pBitrateMbps', '5.25')
    h.setBitrate('compatible1080pBitrateMbps', '9.5')
    await h.advance(600)
    assert.equal(h.calls.length, 1)
    assert.equal(h.calls[0].streamingAv1Preset, 6)
    assert.equal(h.calls[0].streamingAutoResolution, true)
    assert.equal(h.calls[0].compatiblePreset, 'medium')
    assert.equal(h.calls[0].compatible720pBitrateMbps, 5.25)
    assert.equal(h.calls[0].compatible1080pBitrateMbps, 9.5)
    assert.equal(h.calls[0].closeBehavior, 'ask')
    assert.equal(h.calls[0].settingsRevision, 1)
    assert.deepEqual(clone(h.store.settingsCache.value), h.calls[0])
    assert.deepEqual(h.errors, [])
  } finally { h.dispose() }
})

test('compression edits during a slow save are saved after the first request', async () => {
  const h = await settingsHarness()
  try {
    h.holdSave()
    h.form.streamingAv1Preset = 6
    await h.advance(600)
    assert.equal(h.calls.length, 1)
    h.form.compatible1080pBitrateMbps = 12
    await h.advance(600)
    await h.releaseSave()
    assert.equal(h.store.settingsCache.value.compatible1080pBitrateMbps, 8, 'cache must describe the saved snapshot')
    await h.advance(600)
    assert.equal(h.calls.length, 2, 'in-flight edits must not be dropped')
    assert.equal(h.calls[0].compatible1080pBitrateMbps, 8)
    assert.equal(h.calls[1].compatible1080pBitrateMbps, 12)
    assert.deepEqual(clone(h.store.settingsCache.value), h.calls[1])
  } finally { h.dispose() }
})

test('invalid ports pause automatic saving', async () => {
  const h = await settingsHarness()
  try {
    h.form.port = Number.NaN
    h.form.streamingAv1Preset = 6
    await h.advance(600)
    assert.equal(h.calls.length, 0)
    h.form.port = 17001
    await h.advance(600)
    assert.equal(h.calls.length, 1)
  } finally { h.dispose() }
})

test('upstream shortcut updates preserve pending compression edits', async () => {
  const h = await settingsHarness()
  try {
    h.form.streamingAv1Preset = 6
    h.form.compatible720pBitrateMbps = 5
    h.store.settingsCache.value = {
      ...clone(h.store.settingsCache.value),
      closeBehavior: 'tray',
      allowPeerView: true,
      listenMode: 'lan',
      defaultUrlMode: 'lan',
    }
    await h.advance(600)
    assert.equal(h.form.closeBehavior, 'tray')
    assert.equal(h.form.listenMode, 'lan')
    assert.equal(h.form.streamingAv1Preset, 6)
    assert.equal(h.calls.length, 1)
    assert.equal(h.calls[0].compatible720pBitrateMbps, 5)
    assert.equal(h.calls[0].listenMode, 'lan')
    assert.deepEqual(h.errors, [])
  } finally { h.dispose() }
})

test('leaving settings flushes pending compression changes', async () => {
  const h = await settingsHarness()
  h.form.streamingAv1Preset = 6
  await h.advance(100)
  h.dispose()
  await h.advance(0)
  assert.equal(h.calls.length, 1)
  assert.equal(h.calls[0].streamingAv1Preset, 6)
})

test('disabling peer access keeps upstream loopback fallback', async () => {
  const h = await settingsHarness()
  try {
    h.store.settingsCache.value = {
      ...clone(h.store.settingsCache.value),
      allowPeerView: true, listenMode: 'lan', defaultUrlMode: 'lan',
    }
    await h.advance(0)
    h.form.allowPeerView = false
    await h.advance(600)
    assert.equal(h.calls.length, 1)
    assert.equal(h.calls[0].listenMode, 'loopback')
    assert.equal(h.calls[0].defaultUrlMode, 'localhost')
    assert.equal(h.calls[0].allowPeerView, false)
  } finally { h.dispose() }
})
