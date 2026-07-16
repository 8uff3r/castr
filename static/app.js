class CastrApp {
    constructor() {
        this.token = localStorage.getItem('castr_token') || '';
        this.user = JSON.parse(localStorage.getItem('castr_user') || 'null');
        this.streams = [];
        this.currentStreamKey = null;
        this.flvPlayer = null;
        this.chatWs = null;
        this.activeTab = 'login';
        this.currentCategory = 'All';

        this.init();
    }

    async init() {
        try {
            const statusRes = await fetch('/api/auth/status');
            if (statusRes.ok) {
                const status = await statusRes.json();
                this.registrationDisabled = status.registration_disabled;
                if (this.registrationDisabled) {
                    const regTab = document.getElementById('tab-register');
                    if (regTab) regTab.style.display = 'none';
                }
            }
        } catch (e) {}

        if (this.token) {
            try {
                const res = await fetch('/api/auth/me', {
                    headers: { 'Authorization': `Bearer ${this.token}` }
                });
                if (!res.ok) {
                    this.logout();
                }
            } catch (e) {
                console.warn('Could not verify auth token:', e);
            }
        }

        this.updateNavAuth();
        await this.fetchStreams();
        
        // Auto refresh streams every 10 seconds
        setInterval(() => {
            if (document.getElementById('explore-view').classList.contains('active')) {
                this.fetchStreams();
            }
        }, 10000);
    }

    updateNavAuth() {
        const authSection = document.getElementById('auth-nav-section');
        const userSection = document.getElementById('user-nav-section');
        const usernameEl = document.getElementById('nav-username');
        const avatarEl = document.getElementById('nav-user-avatar');

        if (this.token && this.user) {
            authSection.classList.add('hidden');
            userSection.classList.remove('hidden');
            usernameEl.textContent = `@${this.user.username}`;
            avatarEl.textContent = this.user.username[0].toUpperCase();
        } else {
            authSection.classList.remove('hidden');
            userSection.classList.add('hidden');
        }
    }

    async fetchStreams() {
        try {
            const res = await fetch('/api/streams');
            if (!res.ok) throw new Error('Failed to fetch streams');
            this.streams = await res.json();
            this.renderStreams();
            this.updateHeroStats();
        } catch (err) {
            console.error('Error fetching stream directory:', err);
        }
    }

    updateHeroStats() {
        const liveStreams = this.streams.filter(s => s.is_live);
        const totalViewers = liveStreams.reduce((acc, s) => acc + (s.viewer_count || 0), 0);
        
        document.getElementById('stat-active-streams').textContent = liveStreams.length;
        document.getElementById('stat-online-viewers').textContent = totalViewers;
    }

    setCategory(category) {
        this.currentCategory = category;
        document.querySelectorAll('.category-pills .pill').forEach(btn => {
            if (btn.textContent.includes(category) || (category === 'All' && btn.textContent.includes('All'))) {
                btn.classList.add('active');
            } else {
                btn.classList.remove('active');
            }
        });
        this.filterStreams();
    }

    filterStreams() {
        const query = document.getElementById('search-input').value.toLowerCase();
        const filtered = this.streams.filter(s => {
            const matchesQuery = s.title.toLowerCase().includes(query) || 
                                 s.broadcaster.toLowerCase().includes(query) || 
                                 s.stream_key.toLowerCase().includes(query);
            const matchesCategory = this.currentCategory === 'All' || s.category === this.currentCategory;
            return matchesQuery && matchesCategory;
        });
        this.renderStreamGrid(filtered);
    }

    renderStreams() {
        this.filterStreams();
    }

    renderStreamGrid(streamsList) {
        const grid = document.getElementById('streams-grid');
        grid.innerHTML = '';

        if (streamsList.length === 0) {
            grid.innerHTML = `
                <div style="grid-column: 1 / -1; text-align: center; padding: 4rem 1rem; background: var(--bg-card); border-radius: var(--radius-md); border: 1px dashed var(--border-color);">
                    <div style="font-size: 3rem; margin-bottom: 1rem;">📭</div>
                    <h3 style="font-size: 1.3rem; margin-bottom: 0.5rem;">No Active Live Streams Found</h3>
                    <p style="color: var(--text-secondary); max-width: 500px; margin: 0 auto;">
                        There are currently no RTMP webcam broadcasts matching your search. Connect an RTMP client or check back shortly!
                    </p>
                    ${this.token ? `<button class="btn btn-guide" style="margin-top: 1.5rem;" onclick="app.openRtmpModal()">📡 Start RTMP Feed</button>` : ''}
                </div>
            `;
            return;
        }

        streamsList.forEach(stream => {
            const card = document.createElement('div');
            card.className = 'stream-card';
            card.onclick = () => this.watchStream(stream);

            const isLiveBadge = stream.is_live 
                ? `<div class="card-live-badge"><span class="live-dot"></span> LIVE</div>`
                : `<div class="card-live-badge" style="background: hsla(215, 25%, 35%, 0.8);">OFFLINE</div>`;

            card.innerHTML = `
                <div class="stream-thumbnail">
                    ${isLiveBadge}
                    <div class="thumbnail-placeholder">📹</div>
                    <div class="card-viewers-badge">👀 ${stream.viewer_count || 0} watching</div>
                </div>
                <div class="card-content">
                    <div>
                        <div class="card-meta">
                            <span class="card-broadcaster">@${stream.broadcaster}</span>
                            <span class="card-category">${stream.category || 'Live Webcam'}</span>
                        </div>
                        <h3 class="card-title">${stream.title}</h3>
                    </div>
                    <div class="card-footer">
                        <span>Res: ${stream.resolution || '1080p'}</span>
                        <span>Bitrate: ${stream.bitrate_kbps || 2500} kbps</span>
                    </div>
                </div>
            `;
            grid.appendChild(card);
        });
    }

    watchStream(stream) {
        // ENFORCE RESTRICTED WATCHING TO REGISTERED USERS
        if (!this.token || !this.user) {
            this.openAuthModal('login');
            return;
        }

        this.currentStreamKey = stream.stream_key;
        document.getElementById('explore-view').classList.add('hidden');
        document.getElementById('watch-view').classList.remove('hidden');

        // Populate Watch Page Metadata
        document.getElementById('watch-stream-title').textContent = stream.title;
        document.getElementById('watch-broadcaster').textContent = `@${stream.broadcaster}`;
        document.getElementById('watch-avatar').textContent = (stream.broadcaster[0] || 'B').toUpperCase();
        document.getElementById('watch-category').textContent = stream.category || 'Live Webcam';
        document.getElementById('watch-description').textContent = stream.description || 'High-definition live RTMP webcam broadcast.';
        document.getElementById('watch-viewer-count').textContent = stream.viewer_count || 1;
        document.getElementById('stats-fps').textContent = stream.fps || 60;
        document.getElementById('stats-bitrate').textContent = `${stream.bitrate_kbps || 2500} kbps`;
        document.getElementById('stats-res').textContent = stream.resolution || '1920x1080';

        this.startFlvPlayer(stream.stream_key);
        this.connectChat(stream.stream_key);
    }

    startFlvPlayer(streamKey) {
        this.stopFlvPlayer();
        const videoElement = document.getElementById('live-video-player');

        const playerLib = window.mpegts || window.flvjs;
        if (playerLib && playerLib.isSupported()) {
            const flvUrl = `/api/stream/live/${streamKey}.flv?token=${encodeURIComponent(this.token)}`;
            this.flvPlayer = playerLib.createPlayer({
                type: 'flv',
                isLive: true,
                url: flvUrl,
                hasAudio: true,
                hasVideo: true
            }, {
                enableStashBuffer: false,
                stashInitialSize: 128,
                lazyLoad: false
            });

            this.flvPlayer.on(playerLib.Events.ERROR, (errorType, errorDetail, errorInfo) => {
                if (errorInfo && errorInfo.code === 401) {
                    alert("Your viewer session expired or server restarted. Please log in again.");
                    this.logout();
                    this.openAuthModal('login');
                }
            });

            this.flvPlayer.attachMediaElement(videoElement);
            this.flvPlayer.load();
            this.flvPlayer.play().catch(err => {
                console.warn('Auto-play blocked or stream starting:', err);
            });
        } else {
            videoElement.src = `/api/stream/live/${streamKey}.flv?token=${encodeURIComponent(this.token)}`;
            videoElement.play().catch(() => {});
        }
    }

    stopFlvPlayer() {
        if (this.flvPlayer) {
            this.flvPlayer.pause();
            this.flvPlayer.unload();
            this.flvPlayer.detachMediaElement();
            this.flvPlayer.destroy();
            this.flvPlayer = null;
        }
        const videoElement = document.getElementById('live-video-player');
        if (videoElement) {
            videoElement.pause();
            videoElement.src = '';
        }
    }

    connectChat(streamKey) {
        if (this.chatWs) {
            this.chatWs.close();
        }

        const chatContainer = document.getElementById('chat-messages');
        chatContainer.innerHTML = `<div class="system-message">Joining live room chat...</div>`;

        const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
        const wsUrl = `${protocol}//${window.location.host}/api/ws/chat/${streamKey}?token=${encodeURIComponent(this.token)}`;
        this.chatWs = new WebSocket(wsUrl);

        this.chatWs.onopen = () => {
            document.getElementById('chat-connection-status').textContent = '● Connected';
            document.getElementById('chat-connection-status').style.color = 'var(--accent-emerald)';
        };

        this.chatWs.onmessage = (event) => {
            try {
                const msg = JSON.parse(event.data);
                if (msg.error) {
                    this.appendSystemMessage(`Error: ${msg.error}`);
                    return;
                }
                this.appendChatMessage(msg);
            } catch (e) {
                console.error('Chat parse error:', e);
            }
        };

        this.chatWs.onclose = () => {
            document.getElementById('chat-connection-status').textContent = '○ Disconnected';
            document.getElementById('chat-connection-status').style.color = 'var(--accent-red)';
        };
    }

    appendChatMessage(msg) {
        const container = document.getElementById('chat-messages');
        const div = document.createElement('div');
        div.className = 'chat-msg';

        const badgeHtml = msg.badge ? `<span class="chat-badge">${msg.badge}</span>` : '';
        div.innerHTML = `
            <span class="chat-author">${badgeHtml} @${msg.sender}:</span>
            <span class="chat-text">${msg.message}</span>
        `;
        container.appendChild(div);
        container.scrollTop = container.scrollHeight;
    }

    appendSystemMessage(text) {
        const container = document.getElementById('chat-messages');
        const div = document.createElement('div');
        div.className = 'system-message';
        div.textContent = text;
        container.appendChild(div);
        container.scrollTop = container.scrollHeight;
    }

    sendChatMessage(event) {
        event.preventDefault();
        const input = document.getElementById('chat-input');
        const text = input.value.trim();
        if (!text || !this.chatWs || this.chatWs.readyState !== WebSocket.OPEN) return;

        const payload = {
            message: text,
            stream_key: this.currentStreamKey
        };
        this.chatWs.send(JSON.stringify(payload));
        input.value = '';
    }

    insertEmoji(emoji) {
        const input = document.getElementById('chat-input');
        input.value += emoji + ' ';
        input.focus();
    }

    showExplore() {
        this.stopFlvPlayer();
        if (this.chatWs) {
            this.chatWs.close();
            this.chatWs = null;
        }
        document.getElementById('watch-view').classList.add('hidden');
        document.getElementById('explore-view').classList.remove('hidden');
        this.fetchStreams();
    }

    openAuthModal(tab = 'login') {
        if (this.registrationDisabled && tab === 'register') {
            tab = 'login';
        }
        document.getElementById('auth-modal').classList.remove('hidden');
        this.switchAuthTab(tab);
    }

    closeAuthModal() {
        document.getElementById('auth-modal').classList.add('hidden');
        document.getElementById('auth-error-msg').classList.add('hidden');
    }

    switchAuthTab(tab) {
        this.activeTab = tab;
        const loginBtn = document.getElementById('tab-login');
        const regBtn = document.getElementById('tab-register');
        const submitBtn = document.getElementById('auth-submit-btn');
        const titleEl = document.getElementById('auth-modal-title');

        if (tab === 'login') {
            loginBtn.classList.add('active');
            regBtn.classList.remove('active');
            submitBtn.textContent = 'Log In & Watch';
            titleEl.textContent = 'Registered Users Only';
        } else {
            regBtn.classList.add('active');
            loginBtn.classList.remove('active');
            submitBtn.textContent = 'Register & Watch';
            titleEl.textContent = 'Create Viewer Account';
        }
    }

    async handleAuthSubmit(event) {
        event.preventDefault();
        const username = document.getElementById('auth-username').value.trim();
        const password = document.getElementById('auth-password').value;
        const errorEl = document.getElementById('auth-error-msg');

        const endpoint = this.activeTab === 'login' ? '/api/auth/login' : '/api/auth/register';

        try {
            const res = await fetch(endpoint, {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ username, password })
            });

            const data = await res.json();
            if (!res.ok) {
                errorEl.textContent = data.error || 'Authentication failed';
                errorEl.classList.remove('hidden');
                return;
            }

            this.token = data.token;
            this.user = { username: data.username };
            localStorage.setItem('castr_token', this.token);
            localStorage.setItem('castr_user', JSON.stringify(this.user));

            this.updateNavAuth();
            this.closeAuthModal();

            // If user clicked a stream right before auth, open it automatically!
            if (this.currentStreamKey) {
                const stream = this.streams.find(s => s.stream_key === this.currentStreamKey);
                if (stream) {
                    this.watchStream(stream);
                }
            } else {
                this.fetchStreams();
            }
        } catch (err) {
            errorEl.textContent = 'Network error during authentication.';
            errorEl.classList.remove('hidden');
        }
    }

    logout() {
        localStorage.removeItem('castr_token');
        localStorage.removeItem('castr_user');
        this.token = '';
        this.user = null;
        this.updateNavAuth();
        this.showExplore();
    }

    openRtmpModal() {
        document.getElementById('rtmp-modal').classList.remove('hidden');
        if (this.user) {
            document.getElementById('rtmp-key').value = `webcam_${this.user.username}`;
        }
    }

    closeRtmpModal() {
        document.getElementById('rtmp-modal').classList.add('hidden');
    }

    copyText(elementId) {
        const input = document.getElementById(elementId);
        input.select();
        navigator.clipboard.writeText(input.value);
        
        const btn = input.nextElementSibling;
        const oldText = btn.textContent;
        btn.textContent = 'Copied!';
        setTimeout(() => { btn.textContent = oldText; }, 2000);
    }

    copyStreamUrl() {
        const url = window.location.href;
        navigator.clipboard.writeText(url);
        alert('Live Stream Watch URL copied to clipboard!');
    }
}

const app = new CastrApp();
