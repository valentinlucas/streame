// Page téléphone : réglages puis direct. Capture caméra + micro, WebRTC vers le Mac
// (qui fait l'offre), réception du retour audio. Signaling JSON sur WebSocket (/ws).
(() => {
  const $ = (id) => document.getElementById(id);
  const setupStatus = (t) => { $('setup-status').textContent = t; console.log('[streame]', t); };
  const liveStatus = (t) => { $('live-status').textContent = t; console.log('[streame]', t); };

  let ws, pc, stream, wakeLock, retryTimer, statsTimer, prevStats = null;
  let live = false;        // true = direct en cours
  let wantConnected = false;
  let micOn = true, spkOn = true;

  // ---- Contraintes de capture (vidéo figée en 16:9 paysage) -------------------------------
  function constraints() {
    const q = parseInt($('quality').value, 10);
    const dims = { 1080: [1920, 1080], 720: [1280, 720], 480: [854, 480] }[q] || [1280, 720];
    const micId = $('mic').value;
    return {
      audio: Object.assign(
        { echoCancellation: true, noiseSuppression: true, autoGainControl: true },
        micId ? { deviceId: { exact: micId } } : {},
      ),
      video: {
        facingMode: { ideal: $('camera').value },
        width: { ideal: dims[0] }, height: { ideal: dims[1] },
        aspectRatio: { ideal: 16 / 9 }, frameRate: { ideal: 30 },
      },
    };
  }

  async function keepAwake() {
    try { if ('wakeLock' in navigator) wakeLock = await navigator.wakeLock.request('screen'); } catch (e) { /* ignoré */ }
  }
  function send(msg) { if (ws && ws.readyState === 1) ws.send(JSON.stringify(msg)); }

  // ---- Liste des périphériques (caméras et sources audio) ---------------------------------
  async function refreshDevices() {
    let devs = [];
    try { devs = await navigator.mediaDevices.enumerateDevices(); } catch (e) { return; }
    const mics = devs.filter((d) => d.kind === 'audioinput');
    const sel = $('mic');
    const cur = sel.value;
    sel.innerHTML = '<option value="">Micro par défaut</option>';
    mics.forEach((d, i) => {
      const o = document.createElement('option');
      o.value = d.deviceId;
      o.textContent = d.label || `Micro ${i + 1}`;
      sel.appendChild(o);
    });
    if ([...sel.options].some((o) => o.value === cur)) sel.value = cur;
  }

  // ---- Aperçu (écran de réglages) ---------------------------------------------------------
  async function startPreview() {
    $('enable').hidden = true;
    setupStatus('Accès à la caméra…');
    try {
      if (stream) stream.getTracks().forEach((t) => t.stop());
      stream = await navigator.mediaDevices.getUserMedia(constraints());
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
    wantConnected = true;
    live = true;
    micOn = true; spkOn = true;
    localStorage.setItem('streame-name', $('name').value);
    localStorage.setItem('streame-cam', $('camera').value);
    localStorage.setItem('streame-mic', $('mic').value);
    localStorage.setItem('streame-q', $('quality').value);

    $('local').srcObject = stream;
    $('setup').hidden = true;
    $('live').hidden = false;
    updateMuteButtons();
    // Lecture du retour audio déclenchée pendant le geste utilisateur (exigé par iOS).
    $('remote').muted = false;
    $('remote').play().catch(() => {});
    try { if (screen.orientation && screen.orientation.lock) await screen.orientation.lock('landscape'); } catch (e) { /* iOS : ignoré */ }
    keepAwake();
    connectWs();
  }

  function endLive(message) {
    wantConnected = false;
    live = false;
    clearInterval(statsTimer); prevStats = null;
    clearTimeout(retryTimer);
    if (ws) { try { send({ type: 'bye' }); ws.close(); } catch (e) { /* ignoré */ } ws = null; }
    if (pc) { pc.close(); pc = null; }
    try { if (screen.orientation && screen.orientation.unlock) screen.orientation.unlock(); } catch (e) { /* ignoré */ }
    $('remote').muted = true;
    $('live').hidden = true;
    $('setup').hidden = false;
    setupStatus(message || 'Direct arrêté.');
    startPreview(); // relance l'aperçu pour un nouveau départ
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
    if (pc) pc.close();
    pc = new RTCPeerConnection({ iceServers: [{ urls: 'stun:stun.l.google.com:19302' }] });
    window.streamePc = pc; // pour le débogage
    pc.onicecandidate = (e) => { if (e.candidate) send({ type: 'ice', candidate: e.candidate.candidate, sdpMLineIndex: e.candidate.sdpMLineIndex }); };
    pc.ontrack = (e) => { if (e.track.kind === 'audio') { $('remote').srcObject = e.streams[0] || new MediaStream([e.track]); if (spkOn) $('remote').play().catch(() => {}); } };
    pc.onconnectionstatechange = () => {
      liveStatus('WebRTC : ' + pc.connectionState);
      if (pc.connectionState === 'failed') { ws.close(); }
      clearInterval(statsTimer);
      if (pc.connectionState === 'connected') statsTimer = setInterval(() => reportStats().catch(() => {}), 1000);
    };
    await pc.setRemoteDescription({ type: 'offer', sdp });
    // Le Mac propose : audio (bidirectionnel) + vidéo (réception seule chez lui).
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
    // Débit vidéo max (après setLocalDescription, sinon les encodings sont vides).
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

  // Statistiques de l'encodeur et du réseau, affichées ici et envoyées au Mac.
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

  // Relance l'aperçu quand un réglage change.
  ['camera', 'mic', 'quality'].forEach((id) => { $(id).onchange = () => { if (!live) startPreview(); }; });
  navigator.mediaDevices.addEventListener('devicechange', () => { if (!live) refreshDevices(); });
  document.addEventListener('visibilitychange', () => { if (document.visibilityState === 'visible' && live) keepAwake(); });

  // Restaure les derniers réglages puis lance l'aperçu.
  $('name').value = localStorage.getItem('streame-name') || '';
  if (localStorage.getItem('streame-cam')) $('camera').value = localStorage.getItem('streame-cam');
  if (localStorage.getItem('streame-q')) $('quality').value = localStorage.getItem('streame-q');
  startPreview().then(() => {
    const m = localStorage.getItem('streame-mic');
    if (m && [...$('mic').options].some((o) => o.value === m)) { $('mic').value = m; startPreview(); }
  });
})();
