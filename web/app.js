// Page téléphone : capture caméra + micro, WebRTC vers le Mac (qui fait l'offre),
// réception du retour audio. Signaling JSON sur WebSocket (/ws).
(() => {
  const $ = (id) => document.getElementById(id);
  const status = (t) => { $('status').textContent = t; console.log('[streame]', t); };
  let ws, pc, stream, wakeLock, wantConnected = false, retryTimer;

  function constraints() {
    const q = parseInt($('quality').value, 10);
    const dims = { 1080: [1920, 1080], 720: [1280, 720], 480: [854, 480] }[q] || [1280, 720];
    return {
      audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true },
      video: { facingMode: { ideal: $('camera').value }, width: { ideal: dims[0] }, height: { ideal: dims[1] }, frameRate: { ideal: 30 } },
    };
  }

  async function keepAwake() {
    try { if ('wakeLock' in navigator) wakeLock = await navigator.wakeLock.request('screen'); } catch (e) { /* ignoré */ }
  }

  function send(msg) { if (ws && ws.readyState === 1) ws.send(JSON.stringify(msg)); }

  async function start() {
    wantConnected = true;
    localStorage.setItem('streame-name', $('name').value);
    $('connect').disabled = true;
    status('Accès à la caméra…');
    try {
      stream = await navigator.mediaDevices.getUserMedia(constraints());
      stream.getVideoTracks().forEach((t) => { try { t.contentHint = 'motion'; } catch (e) { /* ignoré */ } });
    } catch (e) {
      status('Caméra refusée : ' + e.message + ' (HTTPS requis)');
      $('connect').disabled = false; wantConnected = false;
      return;
    }
    $('local').srcObject = stream;
    $('mute').disabled = false;
    // Déclenche la lecture audio pendant le geste utilisateur (exigé par iOS).
    $('remote').play().catch(() => {});
    keepAwake();
    connectWs();
  }

  function connectWs() {
    clearTimeout(retryTimer);
    status('Connexion au Mac…');
    ws = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws');
    ws.onopen = () => send({ type: 'hello', name: $('name').value || undefined });
    ws.onmessage = async (ev) => {
      const msg = JSON.parse(ev.data);
      if (msg.type === 'offer') await onOffer(msg.sdp);
      else if (msg.type === 'ice') { try { await pc.addIceCandidate({ candidate: msg.candidate, sdpMLineIndex: msg.sdpMLineIndex }); } catch (e) { console.warn(e); } }
      else if (msg.type === 'bye') { status('Session terminée par le Mac : ' + msg.reason); stop(false); }
      else if (msg.type === 'error') status('Erreur : ' + msg.message);
    };
    ws.onclose = () => {
      if (pc) { pc.close(); pc = null; }
      if (wantConnected) { status('Déconnecté, nouvelle tentative…'); retryTimer = setTimeout(connectWs, 2000); }
    };
    ws.onerror = () => {};
  }

  async function onOffer(sdp) {
    if (pc) pc.close();
    pc = new RTCPeerConnection({ iceServers: [{ urls: 'stun:stun.l.google.com:19302' }] });
    window.streamePc = pc; // pour le débogage
    pc.onicecandidate = (e) => { if (e.candidate) send({ type: 'ice', candidate: e.candidate.candidate, sdpMLineIndex: e.candidate.sdpMLineIndex }); };
    pc.ontrack = (e) => { if (e.track.kind === 'audio') { $('remote').srcObject = e.streams[0] || new MediaStream([e.track]); $('remote').play().catch(() => {}); } };
    pc.onconnectionstatechange = () => {
      status('WebRTC : ' + pc.connectionState);
      if (pc.connectionState === 'failed') { ws.close(); }
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
    status('Réponse envoyée, négociation…');
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

  function stop(userInitiated) {
    wantConnected = false;
    clearTimeout(retryTimer);
    if (ws) { try { send({ type: 'bye' }); ws.close(); } catch (e) { /* ignoré */ } ws = null; }
    if (pc) { pc.close(); pc = null; }
    if (stream) { stream.getTracks().forEach((t) => t.stop()); stream = null; }
    $('local').srcObject = null;
    $('connect').textContent = 'Connecter';
    $('connect').disabled = false;
    $('mute').disabled = true;
    if (userInitiated) status('Arrêté.');
  }

  $('connect').onclick = () => {
    if (wantConnected) { stop(true); return; }
    $('connect').textContent = 'Arrêter';
    start().then(() => { $('connect').disabled = false; });
  };
  $('mute').onclick = () => {
    if (!stream) return;
    const a = stream.getAudioTracks()[0];
    if (!a) return;
    a.enabled = !a.enabled;
    $('mute').textContent = a.enabled ? 'Couper le micro' : 'Réactiver le micro';
  };
  $('name').value = localStorage.getItem('streame-name') || '';
  document.addEventListener('visibilitychange', () => { if (document.visibilityState === 'visible' && wantConnected) keepAwake(); });
})();
