// Page téléphone : réglages puis direct. Capture caméra + micro, WebRTC vers le Mac
// (qui fait l'offre), réception du retour audio. Signaling JSON sur WebSocket (/ws).
(() => {
  const $ = (id) => document.getElementById(id);
  const setupStatus = (t) => { $('setup-status').textContent = t; console.log('[streame]', t); };
  const liveStatus = (t) => { $('live-status').textContent = t; console.log('[streame]', t); };

  let ws, pc, stream, wakeLock, retryTimer, statsTimer, prevStats = null;
  let live = false;
  let wantConnected = false;
  let micOn = true, spkOn = true;

  // ---- Contraintes de capture (vidéo figée en 16:9 paysage) -------------------------------
  function constraints() {
    const q = parseInt($('quality').value, 10);
    const dims = { 1080: [1920, 1080], 720: [1280, 720], 480: [854, 480] }[q] || [1280, 720];
    const cam = $('camera').value;
    const micId = $('mic').value;
    const video = Object.assign(
      { width: { ideal: dims[0] }, height: { ideal: dims[1] }, aspectRatio: { ideal: 16 / 9 }, frameRate: { ideal: 30 } },
      cam.startsWith('facing:') ? { facingMode: { ideal: cam.slice(7) } } : { deviceId: { exact: cam } },
    );
    return {
      audio: Object.assign(
        { echoCancellation: true, noiseSuppression: true, autoGainControl: true },
        micId ? { deviceId: { exact: micId } } : {},
      ),
      video,
    };
  }

  async function keepAwake() {
    try { if ('wakeLock' in navigator) wakeLock = await navigator.wakeLock.request('screen'); } catch (e) { /* ignoré */ }
  }
  function send(msg) { if (ws && ws.readyState === 1) ws.send(JSON.stringify(msg)); }

  // ---- Périphériques : objectifs de caméra + sources audio --------------------------------
  async function refreshDevices() {
    let devs = [];
    try { devs = await navigator.mediaDevices.enumerateDevices(); } catch (e) { return; }
    // Caméras : les libellés (« Caméra grand angle arrière », « ultra grand angle »…)
    // n'apparaissent qu'après autorisation. Chaque objectif de l'iPhone est un périphérique.
    const cams = devs.filter((d) => d.kind === 'videoinput');
    const csel = $('camera'), ccur = csel.value;
    csel.innerHTML = '<option value="facing:environment">Arrière</option><option value="facing:user">Avant</option>';
    cams.forEach((d, i) => {
      if (!d.label) return;
      const o = document.createElement('option');
      o.value = d.deviceId; o.textContent = d.label;
      csel.appendChild(o);
    });
    if ([...csel.options].some((o) => o.value === ccur)) csel.value = ccur;

    const mics = devs.filter((d) => d.kind === 'audioinput');
    const msel = $('mic'), mcur = msel.value;
    msel.innerHTML = '<option value="">Micro par défaut</option>';
    mics.forEach((d, i) => {
      const o = document.createElement('option');
      o.value = d.deviceId; o.textContent = d.label || `Micro ${i + 1}`;
      msel.appendChild(o);
    });
    if ([...msel.options].some((o) => o.value === mcur)) msel.value = mcur;

    // Sorties audio (retour du Mac) : sélectionnables via setSinkId (Chrome/Brave/Edge/Firefox,
    // desktop et Android). iOS/WebKit ne l'expose pas : la sortie suit la route système.
    const ssel = $('sink');
    if (!canPickSink()) {
      ssel.hidden = true; $('sink-label').hidden = true; $('sink-hint').hidden = false;
    } else {
      const outs = devs.filter((d) => d.kind === 'audiooutput');
      const scur = ssel.value;
      ssel.innerHTML = '<option value="">Sortie par défaut</option>';
      outs.forEach((d, i) => {
        const o = document.createElement('option');
        o.value = d.deviceId; o.textContent = d.label || `Sortie ${i + 1}`;
        ssel.appendChild(o);
      });
      if ([...ssel.options].some((o) => o.value === scur)) ssel.value = scur;
    }
  }

  function canPickSink() { return typeof HTMLMediaElement.prototype.setSinkId === 'function'; }

  // Applique la sortie choisie à l'élément qui joue le retour du Mac.
  async function applySink() {
    if (!canPickSink()) return;
    const id = $('sink').value;
    try { await $('remote').setSinkId(id || ''); }
    catch (e) { console.warn('setSinkId', e); setupStatus('Sortie audio indisponible : ' + (e.message || e)); }
  }

  // Changement de micro pendant le direct : nouvelle capture audio, puis on remplace la piste
  // envoyée au Mac sans renégocier (replaceTrack). L'ancienne piste est arrêtée.
  async function switchMic() {
    if (!live || !stream || !pc) return;
    let fresh;
    try { fresh = await navigator.mediaDevices.getUserMedia({ audio: constraints().audio }); }
    catch (e) { liveStatus('Micro indisponible : ' + (e.message || e)); return; }
    const track = fresh.getAudioTracks()[0];
    if (!track) return;
    track.enabled = micOn;
    const sender = pc.getSenders().find((s) => s.track && s.track.kind === 'audio');
    if (sender) { try { await sender.replaceTrack(track); } catch (e) { console.warn('replaceTrack', e); } }
    stream.getAudioTracks().forEach((t) => { t.stop(); stream.removeTrack(t); });
    stream.addTrack(track);
    localStorage.setItem('streame-mic', $('mic').value);
    liveStatus('Micro : ' + (track.label || 'changé'));
  }

  // ---- Aperçu (écran de réglages) ---------------------------------------------------------
  async function startPreview() {
    $('enable').hidden = true;
    setupStatus('Accès à la caméra…');
    try {
      if (stream) stream.getTracks().forEach((t) => t.stop());
      try {
        stream = await navigator.mediaDevices.getUserMedia(constraints());
      } catch (e) {
        // Périphérique choisi disparu (AirPods rangés, objectif indisponible…) : on repart sur
        // les périphériques par défaut plutôt que de bloquer l'écran de réglages.
        if (!['OverconstrainedError', 'NotFoundError', 'NotReadableError'].includes(e.name)) throw e;
        console.warn('[streame] périphérique indisponible, repli par défaut :', e.name);
        $('mic').value = ''; $('camera').value = 'facing:environment';
        stream = await navigator.mediaDevices.getUserMedia(constraints());
      }
      stream.getVideoTracks().forEach((t) => { try { t.contentHint = 'motion'; } catch (e) { /* ignoré */ } });
    } catch (e) {
      stream = null;
      $('enable').hidden = false;
      setupStatus('Caméra/micro refusés : ' + (e.message || e) + ' (HTTPS requis)');
      return false;
    }
    $('preview').srcObject = stream;
    await refreshDevices();
    const v = stream.getVideoTracks()[0];
    const s = v ? v.getSettings() : {};
    setupStatus(`Prêt · ${s.width || '?'}x${s.height || '?'} · ${v ? v.label : ''}`);
    return true;
  }

  // ---- Passage en direct ------------------------------------------------------------------
  async function goLive() {
    if (!stream && !(await startPreview())) return;
    wantConnected = true; live = true; micOn = true; spkOn = true;
    localStorage.setItem('streame-name', $('name').value);
    localStorage.setItem('streame-cam', $('camera').value);
    localStorage.setItem('streame-mic', $('mic').value);
    localStorage.setItem('streame-sink', $('sink').value);
    localStorage.setItem('streame-q', $('quality').value);

    $('local').srcObject = stream;
    $('setup').hidden = true;
    $('live').hidden = false;
    setOnAir(false);
    updateMuteButtons();
    $('remote').muted = false;
    applySink();
    $('remote').play().catch(() => {});
    requestFullscreen(); // geste utilisateur : plein écran sur Android/desktop
    keepAwake();
    connectWs();
  }

  function endLive(message) {
    wantConnected = false; live = false;
    clearInterval(statsTimer); prevStats = null;
    clearTimeout(retryTimer);
    if (ws) { try { send({ type: 'bye' }); ws.close(); } catch (e) { /* ignoré */ } ws = null; }
    if (pc) { pc.close(); pc = null; }
    setOnAir(false);
    $('remote').muted = true;
    $('live').hidden = true;
    $('setup').hidden = false;
    setupStatus(message || 'Direct arrêté.');
    startPreview();
  }

  // ---- Signaling / WebRTC -----------------------------------------------------------------
  function connectWs() {
    clearTimeout(retryTimer);
    liveStatus('Connexion au Mac…');
    ws = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws');
    ws.onopen = () => send({ type: 'hello', name: $('name').value || undefined });
    ws.onmessage = async (ev) => {
      const msg = JSON.parse(ev.data);
      if (msg.type === 'offer') await onOffer(msg.sdp);
      else if (msg.type === 'ice') { try { await pc.addIceCandidate({ candidate: msg.candidate, sdpMLineIndex: msg.sdpMLineIndex }); } catch (e) { console.warn(e); } }
      else if (msg.type === 'on_air') setOnAir(msg.on);
      else if (msg.type === 'bye') { endLive('Session terminée par le Mac.'); }
      else if (msg.type === 'error') liveStatus('Erreur : ' + msg.message);
    };
    ws.onclose = () => {
      if (pc) { pc.close(); pc = null; }
      if (wantConnected) { liveStatus('Déconnecté, nouvelle tentative…'); retryTimer = setTimeout(connectWs, 2000); }
    };
    ws.onerror = () => {};
  }

  async function onOffer(sdp) {
    // Ré-offre du Mac (redémarrage ICE après un échec) : on garde la même RTCPeerConnection,
    // ses pistes, son DTLS et les décodeurs côté Mac — coupure bien plus courte qu'une
    // reconnexion complète.
    if (pc && pc.remoteDescription && pc.signalingState === 'stable' && pc.connectionState !== 'closed') {
      try {
        await pc.setRemoteDescription({ type: 'offer', sdp });
        const answer = await pc.createAnswer();
        await pc.setLocalDescription(answer);
        send({ type: 'answer', sdp: answer.sdp });
        liveStatus('Redémarrage ICE…');
        return;
      } catch (e) { console.warn('[streame] ré-offre impossible, reconnexion complète', e); }
    }
    if (pc) pc.close();
    pc = new RTCPeerConnection({ iceServers: [{ urls: 'stun:stun.l.google.com:19302' }] });
    window.streamePc = pc; // pour le débogage
    pc.oniceconnectionstatechange = () => console.log('[streame] ICE :', pc.iceConnectionState);
    pc.onicecandidate = (e) => { if (e.candidate) send({ type: 'ice', candidate: e.candidate.candidate, sdpMLineIndex: e.candidate.sdpMLineIndex }); };
    pc.ontrack = (e) => { if (e.track.kind === 'audio') { $('remote').srcObject = e.streams[0] || new MediaStream([e.track]); if (spkOn) $('remote').play().catch(() => {}); } };
    pc.onconnectionstatechange = () => {
      liveStatus('WebRTC : ' + pc.connectionState);
      if (pc.connectionState === 'failed') { ws.close(); }
      clearInterval(statsTimer);
      if (pc.connectionState === 'connected') statsTimer = setInterval(() => reportStats().catch(() => {}), 1000);
    };
    await pc.setRemoteDescription({ type: 'offer', sdp });
    for (const t of pc.getTransceivers()) {
      const kind = t.receiver.track ? t.receiver.track.kind : (t.mid === '0' ? 'audio' : 'video');
      const track = stream.getTracks().find((x) => x.kind === kind);
      if (!track) continue;
      await t.sender.replaceTrack(track);
      t.direction = kind === 'audio' ? 'sendrecv' : 'sendonly';
    }
    const answer = await pc.createAnswer();
    await pc.setLocalDescription(answer);
    send({ type: 'answer', sdp: answer.sdp });
    liveStatus('Négociation…');
    for (const s of pc.getSenders()) {
      if (!s.track || s.track.kind !== 'video') continue;
      try {
        const p = s.getParameters();
        if (p.encodings && p.encodings.length) {
          const q = parseInt($('quality').value, 10);
          p.encodings[0].maxBitrate = { 1080: 8_000_000, 720: 4_500_000, 480: 2_000_000 }[q] || 4_000_000;
          p.degradationPreference = 'maintain-resolution';
          await s.setParameters(p);
        }
      } catch (e) { console.warn('setParameters', e); }
    }
  }

  async function reportStats() {
    if (!pc || pc.connectionState !== 'connected') return;
    let out = null, rtt = null, codecs = {};
    const report = await pc.getStats();
    report.forEach((s) => { if (s.type === 'codec') codecs[s.id] = s.mimeType; });
    report.forEach((s) => {
      if (s.type === 'outbound-rtp' && s.kind === 'video') out = s;
      if (s.type === 'candidate-pair' && s.state === 'succeeded' && s.currentRoundTripTime != null) rtt = s.currentRoundTripTime * 1000;
    });
    if (!out) return;
    const now = performance.now();
    let kbps = 0;
    if (prevStats && out.bytesSent != null) kbps = (out.bytesSent - prevStats.bytes) * 8 / ((now - prevStats.at) / 1000) / 1000;
    prevStats = { bytes: out.bytesSent || 0, at: now };
    const st = {
      type: 'stats',
      width: out.frameWidth || 0, height: out.frameHeight || 0,
      fps: out.framesPerSecond || 0, bitrate_kbps: kbps,
      quality_limitation: out.qualityLimitationReason || 'inconnue',
      rtt_ms: rtt, codec: (codecs[out.codecId] || '').replace('video/', ''),
    };
    send(st);
    liveStatus(`● ${st.width}x${st.height} · ${Math.round(st.fps)} i/s · ${(kbps / 1000).toFixed(1)} Mb/s` + (rtt != null ? ` · ${Math.round(rtt)} ms` : ''));
  }

  // ---- Retour « à l'antenne » -------------------------------------------------------------
  function setOnAir(on) {
    $('live').classList.toggle('onair-active', on);
    $('onair').hidden = !on;
  }

  // ---- Boutons du direct ------------------------------------------------------------------
  function updateMuteButtons() {
    const mic = $('mute'), spk = $('spk');
    mic.classList.toggle('off', !micOn);
    mic.querySelector('.ico').textContent = micOn ? '🎙' : '🔇';
    mic.querySelector('.lbl').textContent = micOn ? 'Micro' : 'Coupé';
    spk.classList.toggle('off', !spkOn);
    spk.querySelector('.ico').textContent = spkOn ? '🔊' : '🔈';
    spk.querySelector('.lbl').textContent = spkOn ? 'Son' : 'Coupé';
  }

  $('mute').onclick = () => {
    if (!stream) return;
    micOn = !micOn;
    stream.getAudioTracks().forEach((t) => { t.enabled = micOn; });
    updateMuteButtons();
  };
  $('spk').onclick = () => {
    spkOn = !spkOn;
    const a = $('remote');
    a.muted = !spkOn;
    if (spkOn) a.play().catch(() => {});
    updateMuteButtons();
  };
  $('quit').onclick = () => endLive('Direct arrêté.');
  $('start').onclick = () => goLive();
  $('enable').onclick = () => startPreview();

  // ---- Plein écran ------------------------------------------------------------------------
  function isStandalone() {
    return window.matchMedia('(display-mode: fullscreen)').matches || window.navigator.standalone === true;
  }
  async function requestFullscreen() {
    try { if (!document.fullscreenElement && document.documentElement.requestFullscreen) await document.documentElement.requestFullscreen({ navigationUI: 'hide' }); } catch (e) { /* ignoré */ }
  }
  $('fs').onclick = async () => {
    if (document.fullscreenElement) { try { await document.exitFullscreen(); } catch (e) { /* ignoré */ } return; }
    if (document.documentElement.requestFullscreen) { requestFullscreen(); return; }
    $('fs-hint').hidden = false; // iOS : pas d'API plein écran pour la page
  };
  if (isStandalone()) $('fs').hidden = true;
  else if (!document.documentElement.requestFullscreen) $('fs-hint').hidden = false;

  // ---- Verrouillage paysage (l'image ne dépend plus de l'orientation) ---------------------
  const portraitMq = window.matchMedia('(orientation: portrait)');
  const updateRotate = () => { $('rotate').hidden = !portraitMq.matches; };
  portraitMq.addEventListener('change', updateRotate);
  window.addEventListener('resize', updateRotate);
  updateRotate();

  // ---- Divers -----------------------------------------------------------------------------
  ['camera', 'quality'].forEach((id) => { $(id).onchange = () => { if (!live) startPreview(); }; });
  $('mic').onchange = () => { if (live) switchMic(); else startPreview(); };
  $('sink').onchange = () => { localStorage.setItem('streame-sink', $('sink').value); applySink(); };
  navigator.mediaDevices.addEventListener('devicechange', () => { if (!live) refreshDevices(); });
  document.addEventListener('visibilitychange', () => { if (document.visibilityState === 'visible' && live) keepAwake(); });

  $('name').value = localStorage.getItem('streame-name') || '';
  if (localStorage.getItem('streame-cam')) $('camera').value = localStorage.getItem('streame-cam');
  if (localStorage.getItem('streame-q')) $('quality').value = localStorage.getItem('streame-q');
  startPreview().then(() => {
    for (const [k, id] of [['streame-cam', 'camera'], ['streame-mic', 'mic'], ['streame-sink', 'sink']]) {
      const v = localStorage.getItem(k);
      if (v && [...$(id).options].some((o) => o.value === v)) $(id).value = v;
    }
  });
})();
