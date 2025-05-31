#!/usr/bin/env python3
"""
Windows Volume Control Server
Receives HTTP requests to adjust system volume
"""

import json
import logging
from flask import Flask, request, jsonify
from flask_cors import CORS
import sys

try:
    from pycaw.pycaw import AudioUtilities, IAudioEndpointVolume
    from ctypes import cast, POINTER
    from comtypes import CLSCTX_ALL
    PYCAW_AVAILABLE = True
except ImportError:
    print("Warning: pycaw not available. Install with: pip install pycaw")
    PYCAW_AVAILABLE = False

app = Flask(__name__)
CORS(app)  # Allow cross-origin requests

# Configure logging
logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

class VolumeController:
    def __init__(self):
        self.volume_interface = None
        self.current_volume = 0.5
        self._initialize_audio()
    
    def _initialize_audio(self):
        """Initialize Windows audio interface"""
        if not PYCAW_AVAILABLE:
            logger.error("pycaw library not available")
            return False
            
        try:
            # Get the default audio device
            devices = AudioUtilities.GetSpeakers()
            interface = devices.Activate(IAudioEndpointVolume._iid_, CLSCTX_ALL, None)
            self.volume_interface = cast(interface, POINTER(IAudioEndpointVolume))
            
            # Get current volume
            self.current_volume = self.volume_interface.GetMasterScalarVolume()
            logger.info(f"Audio interface initialized. Current volume: {self.current_volume:.2f}")
            return True
            
        except Exception as e:
            logger.error(f"Failed to initialize audio interface: {e}")
            return False
    
    def set_volume(self, level):
        """Set system volume (0.0 to 1.0)"""
        if not self.volume_interface:
            logger.error("Audio interface not initialized")
            return False
            
        try:
            # Clamp volume to safe range
            level = max(0.0, min(1.0, level))
            
            # Set the volume
            self.volume_interface.SetMasterScalarVolume(level, None)
            self.current_volume = level
            
            logger.info(f"Volume set to {level:.2f}")
            return True
            
        except Exception as e:
            logger.error(f"Failed to set volume: {e}")
            return False
    
    def get_volume(self):
        """Get current system volume"""
        if not self.volume_interface:
            return self.current_volume
            
        try:
            self.current_volume = self.volume_interface.GetMasterScalarVolume()
            return self.current_volume
        except Exception as e:
            logger.error(f"Failed to get volume: {e}")
            return self.current_volume

# Global volume controller
volume_controller = VolumeController()

@app.route('/volume', methods=['POST'])
def set_volume():
    """Set system volume via POST request"""
    try:
        data = request.get_json()
        if not data or 'level' not in data:
            return jsonify({'error': 'Missing level parameter'}), 400
        
        level = float(data['level'])
        if not (0.0 <= level <= 1.0):
            return jsonify({'error': 'Level must be between 0.0 and 1.0'}), 400
        
        success = volume_controller.set_volume(level)
        if success:
            return jsonify({
                'status': 'success',
                'level': level,
                'message': f'Volume set to {level:.2f}'
            })
        else:
            return jsonify({'error': 'Failed to set volume'}), 500
            
    except ValueError:
        return jsonify({'error': 'Invalid level value'}), 400
    except Exception as e:
        logger.error(f"Error in set_volume: {e}")
        return jsonify({'error': str(e)}), 500

@app.route('/volume', methods=['GET'])
def get_volume():
    """Get current system volume"""
    try:
        current_level = volume_controller.get_volume()
        return jsonify({
            'level': current_level,
            'message': f'Current volume: {current_level:.2f}'
        })
    except Exception as e:
        logger.error(f"Error in get_volume: {e}")
        return jsonify({'error': str(e)}), 500

@app.route('/health', methods=['GET'])
def health_check():
    """Health check endpoint"""
    return jsonify({
        'status': 'healthy',
        'audio_initialized': volume_controller.volume_interface is not None
    })

@app.route('/', methods=['GET'])
def index():
    """Simple index page"""
    return '''
    <h1>Windows Volume Control Server</h1>
    <p>Endpoints:</p>
    <ul>
        <li>POST /volume - Set volume (JSON: {"level": 0.0-1.0})</li>
        <li>GET /volume - Get current volume</li>
        <li>GET /health - Health check</li>
    </ul>
    '''

if __name__ == '__main__':
    print("Windows Volume Control Server")
    print("=============================")
    
    if not PYCAW_AVAILABLE:
        print("ERROR: pycaw library not found!")
        print("Install it with: pip install pycaw")
        sys.exit(1)
    
    # Test audio initialization
    if volume_controller.volume_interface:
        print(f"✓ Audio interface initialized successfully")
        print(f"✓ Current system volume: {volume_controller.get_volume():.2f}")
    else:
        print("✗ Failed to initialize audio interface")
        print("Make sure you're running this on Windows with audio devices available")
    
    print("\nStarting server on http://0.0.0.0:8080")
    print("Press Ctrl+C to stop")
    
    try:
        app.run(host='0.0.0.0', port=8080, debug=False)
    except KeyboardInterrupt:
        print("\nServer stopped")
    except Exception as e:
        print(f"Server error: {e}")
