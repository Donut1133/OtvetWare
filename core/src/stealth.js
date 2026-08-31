// stealth.js — main-world инжект отпечатка.
//
// Правится осторожно: если шум ляжет иначе, один и тот же аккаунт станет
// отдавать разный canvas-хэш — то есть ровно тот скачок, ради предотвращения
// которого всё это написано.
//
// На вход идёт cfg: screen, hardwareConcurrency, deviceMemory, maxTouchPoints,
// languages, gpu, connection, noise. Собирается в core/src/cdp.rs из персоны.
function payload(cfg) {
  'use strict';

  const applied = new WeakSet();

  function apply(W) {
    if (!W || applied.has(W)) return;
    applied.add(W);

    // ── маскировка самих патчей ──
    // Любая подменённая функция обязана на .toString() отдавать "[native code]".
    // Держим соответствие «подделка → оригинал» и один раз подменяем
    // Function.prototype.toString, который сам себя тоже отдаёт нативным.
    const nativeToString = W.Function.prototype.toString;
    const origins = new W.WeakMap();
    const fnToString = function toString() {
      const real = origins.get(this);
      return nativeToString.call(real || this);
    };
    origins.set(fnToString, nativeToString);
    try { W.Function.prototype.toString = fnToString; } catch { /* заморожено — не критично */ }

    function native(fake, real, name) {
      try {
        origins.set(fake, real);
        if (name) W.Object.defineProperty(fake, 'name', { value: name, configurable: true });
        // .length должен совпадать с оригиналом — иначе видно по arity
        if (real && typeof real.length === 'number')
          W.Object.defineProperty(fake, 'length', { value: real.length, configurable: true });
      } catch { /* ignore */ }
      return fake;
    }

    // Подменяем свойство, СОХРАНЯЯ форму дескриптора: геттер остаётся геттером,
    // data-property — data-property, флаги enumerable/configurable — как были.
    // Иначе getOwnPropertyDescriptor покажет подмену даже без чтения значения.
    function override(obj, prop, value) {
      try {
        const desc = W.Object.getOwnPropertyDescriptor(obj, prop);
        if (!desc) return false;                       // свойства нет — не выдумываем новое
        if (desc.get) {
          const getter = function () { return value; };
          native(getter, desc.get, 'get ' + prop);
          W.Object.defineProperty(obj, prop, {
            get: getter, set: desc.set,
            enumerable: desc.enumerable, configurable: desc.configurable,
          });
          return true;
        }
        if ('value' in desc && desc.configurable) {
          W.Object.defineProperty(obj, prop, {
            value, writable: desc.writable,
            enumerable: desc.enumerable, configurable: desc.configurable,
          });
          return true;
        }
      } catch { /* ignore */ }
      return false;
    }

    function method(obj, prop, make) {
      try {
        const orig = obj[prop];
        if (typeof orig !== 'function') return false;
        const fake = make(orig);
        native(fake, orig, prop);
        obj[prop] = fake;
        return true;
      } catch { /* ignore */ }
      return false;
    }

    // Детерминированный ГПСЧ (mulberry32). Тот же алгоритм, что в fingerprint.js.
    function rng(seed) {
      let a = seed >>> 0;
      return function () {
        a = (a + 0x6D2B79F5) | 0;
        let t = Math.imul(a ^ (a >>> 15), 1 | a);
        t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
        return (t ^ (t >>> 14)) >>> 0;
      };
    }

    const N = W.navigator, S = W.screen;

    // ── navigator ──
    try {
      const NP = W.Navigator.prototype;
      // hardwareConcurrency и languages в норме уже выставлены через CDP —
      // трогаем ТОЛЬКО если оверрайд не доехал (гонка на первом документе,
      // старый протокол). Лишний JS-патч там, где сработал нативный, — минус.
      if (N.hardwareConcurrency !== cfg.hardwareConcurrency)
        override(NP, 'hardwareConcurrency', cfg.hardwareConcurrency);
      // deviceMemory нативного оверрайда не имеет — только так.
      if ('deviceMemory' in N) override(NP, 'deviceMemory', cfg.deviceMemory);
      if (N.maxTouchPoints !== cfg.maxTouchPoints)
        override(NP, 'maxTouchPoints', cfg.maxTouchPoints);
      const langs = cfg.languages || [];
      if (langs.length && String(N.languages) !== String(langs)) {
        override(NP, 'languages', W.Object.freeze(langs.slice()));
        override(NP, 'language', langs[0]);
      }
      // webdriver при --disable-blink-features=AutomationControlled и так false.
      // Патчим только если он всё-таки true — иначе добавили бы след на ровном месте.
      if (N.webdriver === true) override(NP, 'webdriver', false);
    } catch { /* ignore */ }

    // ── screen ──
    // Размеры окна (outer*/screenX/Y) НЕ трогаем: в headed они настоящие, их задаёт
    // --window-size. Подмена сделала бы их несогласованными с реальным окном.
    try {
      const SP = W.Screen.prototype, c = cfg.screen;
      override(SP, 'width', c.width);
      override(SP, 'height', c.height);
      override(SP, 'availWidth', c.availWidth);
      override(SP, 'availHeight', c.availHeight);
      override(SP, 'availLeft', 0);
      override(SP, 'availTop', 0);
      override(SP, 'colorDepth', c.colorDepth);
      override(SP, 'pixelDepth', c.pixelDepth);
      // Окно физически не может быть больше рабочей области, и пара
      // «outerHeight > availHeight» — готовый признак подделки. В headed окно и так
      // влезает (--window-size задан с запасом), а в headless playwright приписывает
      // к вьюпорту высоту «хрома» — и получается окно выше рабочего стола.
      // Заодно чиним нули: outerWidth === 0 сам по себе выдаёт headless.
      const ow = W.outerWidth || W.innerWidth || c.width;
      const oh = W.outerHeight || ((W.innerHeight || c.height) + 74);
      if (ow !== W.outerWidth || ow > c.availWidth) override(W, 'outerWidth', Math.min(ow, c.availWidth));
      if (oh !== W.outerHeight || oh > c.availHeight) override(W, 'outerHeight', Math.min(oh, c.availHeight));
    } catch { /* ignore */ }

    // ── navigator.connection ──
    try {
      if (N.connection && cfg.connection) {
        const CP = W.Object.getPrototypeOf(N.connection);
        override(CP, 'effectiveType', cfg.connection.effectiveType);
        override(CP, 'rtt', cfg.connection.rtt);
        override(CP, 'downlink', cfg.connection.downlink);
        override(CP, 'saveData', cfg.connection.saveData);
      }
    } catch { /* ignore */ }

    // ── WebGL ──
    // Подменяем ТОЛЬКО UNMASKED_*: это и есть реальная модель GPU. Обычные
    // VENDOR/RENDERER Chrome всегда отдаёт как "WebKit"/"WebKit WebGL" —
    // если подменить и их, получится значение, которого не бывает.
    try {
      const UNMASKED_VENDOR = 0x9245, UNMASKED_RENDERER = 0x9246;
      const patchGl = (Ctor) => {
        if (!Ctor || !Ctor.prototype) return;
        method(Ctor.prototype, 'getParameter', (orig) => function getParameter(p) {
          if (p === UNMASKED_VENDOR) return cfg.gpu.vendor;
          if (p === UNMASKED_RENDERER) return cfg.gpu.renderer;
          return orig.apply(this, arguments);
        });
      };
      patchGl(W.WebGLRenderingContext);
      patchGl(W.WebGL2RenderingContext);
    } catch { /* ignore */ }

    // ── canvas ──
    // Шум ДЕТЕРМИНИРОВАННЫЙ: одинаковый рисунок → одинаковый хэш, всегда.
    // Случайный шум на каждый вызов ломает это и палится проверкой
    // «нарисуй дважды, сравни» — у настоящего браузера хэши совпадают.
    const clamp8 = (v) => (v < 0 ? 0 : v > 255 ? 255 : v);
    function noisify(img) {
      try {
        const d = img.data, n = d.length;
        if (!n) return img;
        const r = rng((cfg.noise.canvas ^ (img.width * 2654435761 + img.height)) >>> 0);
        // Разреженный проход простым шагом: дёшево на больших холстах и не
        // попадает в резонанс с регулярными узорами.
        for (let i = 0; i < n; i += 4 * 71) {
          const v = r();
          if (d[i + 3] === 0) continue;      // прозрачный пиксель не трогаем:
                                             // пустой canvas обязан остаться пустым
          d[i]     = clamp8(d[i]     + (v % 3) - 1);
          d[i + 1] = clamp8(d[i + 1] + ((v >> 8) % 3) - 1);
          d[i + 2] = clamp8(d[i + 2] + ((v >> 16) % 3) - 1);
        }
      } catch { /* ignore */ }
      return img;
    }

    try {
      const C2D = W.CanvasRenderingContext2D && W.CanvasRenderingContext2D.prototype;
      const rawGetImageData = C2D && C2D.getImageData;

      if (C2D) {
        method(C2D, 'getImageData', (orig) => function getImageData() {
          return noisify(orig.apply(this, arguments));
        });
      }

      // Для toDataURL/toBlob шумим КОПИЮ: исходный холст на экране не портим.
      // drawImage принимает и 2d-, и webgl-холст, поэтому путь один на оба.
      const noisyCopy = (canvas) => {
        const w = canvas.width, h = canvas.height;
        if (!w || !h || !rawGetImageData) return null;
        const copy = W.document.createElement('canvas');
        copy.width = w; copy.height = h;
        const ctx = copy.getContext('2d');
        if (!ctx) return null;
        ctx.drawImage(canvas, 0, 0);
        // rawGetImageData — оригинал, иначе шум наложился бы дважды
        const img = rawGetImageData.call(ctx, 0, 0, w, h);
        noisify(img);
        ctx.putImageData(img, 0, 0);
        return copy;
      };

      const HC = W.HTMLCanvasElement && W.HTMLCanvasElement.prototype;
      if (HC) {
        method(HC, 'toDataURL', (orig) => function toDataURL() {
          try { const c = noisyCopy(this); if (c) return orig.apply(c, arguments); }
          catch { /* падать нельзя — отдаём честный результат */ }
          return orig.apply(this, arguments);
        });
        method(HC, 'toBlob', (orig) => function toBlob() {
          try { const c = noisyCopy(this); if (c) return orig.apply(c, arguments); }
          catch { /* ignore */ }
          return orig.apply(this, arguments);
        });
      }
    } catch { /* ignore */ }

    // ── audio ──
    // getChannelData обязан отдавать ТУ ЖЕ ссылку на Float32Array, что и оригинал
    // (проверяется как a === b), поэтому шумим массив на месте и ровно один раз.
    try {
      const noised = new W.WeakSet();
      const AB = W.AudioBuffer && W.AudioBuffer.prototype;
      if (AB) {
        method(AB, 'getChannelData', (orig) => function getChannelData() {
          const arr = orig.apply(this, arguments);
          try {
            if (arr && !noised.has(arr)) {
              noised.add(arr);
              const r = rng(cfg.noise.audio);
              // Шум ОТНОСИТЕЛЬНЫЙ, а не абсолютный. Массив — Float32Array, шаг
              // представимых значений там ≈ v·1.19e-7, поэтому фиксированная
              // добавка 1e-7 рядом с 1.0 просто округляется в ноль и отпечаток не
              // меняется вовсе (проверено: суммы двух разных персон совпадали до
              // последнего знака). Множитель 1e-5 переживает округление при ЛЮБОЙ
              // величине сэмпла, неслышим (-100 дБ) и оставляет тишину тишиной:
              // ноль умножается в ноль, а «шумящая тишина» — сама по себе признак.
              // Шаг 17: отпечаток часто считают по короткому срезу, и редкий шум
              // в этот срез может не попасть.
              for (let i = 0; i < arr.length; i += 17)
                arr[i] *= 1 + (r() / 4294967296 - 0.5) * 2e-5;
            }
          } catch { /* ignore */ }
          return arr;
        });
      }
      const AN = W.AnalyserNode && W.AnalyserNode.prototype;
      if (AN) {
        method(AN, 'getFloatFrequencyData', (orig) => function getFloatFrequencyData(arr) {
          const res = orig.apply(this, arguments);
          try {
            const r = rng(cfg.noise.audio);
            for (let i = 0; i < arr.length; i += 23)
              arr[i] += (r() / 4294967296 - 0.5) * 2e-4;
          } catch { /* ignore */ }
          return res;
        });
      }
    } catch { /* ignore */ }

    // ── permissions ──
    // Классический признак headless: Notification.permission === 'denied',
    // а permissions.query для 'notifications' отвечает 'prompt'. У живого
    // браузера так не бывает.
    try {
      // Патчим ПРОТОТИП, а не navigator.permissions: присваивание экземпляру
      // создало бы собственное свойство, которое видно в getOwnPropertyNames.
      const PP = W.Permissions && W.Permissions.prototype;
      if (PP && PP.query && W.Notification) {
        method(PP, 'query', (orig) => function query(desc) {
          const p = orig.apply(this, arguments);
          try {
            if (!desc || desc.name !== 'notifications') return p;
            return p.then((status) => {
              // Notification.permission даёт 'default'|'granted'|'denied', а
              // PermissionState — 'prompt'|'granted'|'denied'. Отдавать 'default'
              // нельзя: такого значения у живого браузера не бывает вообще.
              const want = W.Notification.permission === 'default' ? 'prompt' : W.Notification.permission;
              // Правим ТОЛЬКО при реальном расхождении (это headless-признак);
              // в headed значения и так совпадают, и патч не сработает.
              if (status && status.state !== want) {
                try { W.Object.defineProperty(status, 'state', { get: () => want, configurable: true }); } catch { /* ignore */ }
              }
              return status;   // остаётся настоящим PermissionStatus (EventTarget, instanceof)
            });
          } catch { /* ignore */ }
          return p;
        });
      }
    } catch { /* ignore */ }

    // ── iframe ──
    // У фрейма СВОЙ realm: Navigator.prototype там другой объект, и все патчи выше
    // на него не действуют. Достаточно вставить about:blank-фрейм и прочитать
    // настоящие deviceMemory/GPU/canvas. Поэтому доклеиваем payload на фрейм в
    // момент обращения к нему — тем же кодом, только с другим W.
    try {
      const IP = W.HTMLIFrameElement && W.HTMLIFrameElement.prototype;
      if (IP) {
        const hook = (prop, pick) => {
          const desc = W.Object.getOwnPropertyDescriptor(IP, prop);
          if (!desc || !desc.get) return;
          const orig = desc.get;
          const getter = function () {
            const out = orig.call(this);
            try { const w = pick(out); if (w && w.Navigator) apply(w); } catch { /* чужой origin — не наше дело */ }
            return out;
          };
          native(getter, orig, 'get ' + prop);
          W.Object.defineProperty(IP, prop, {
            get: getter, set: desc.set,
            enumerable: desc.enumerable, configurable: desc.configurable,
          });
        };
        hook('contentWindow', (w) => w);
        hook('contentDocument', (d) => d && d.defaultView);
      }
    } catch { /* ignore */ }
  }

  try { apply(window); } catch { /* ignore */ }
}
