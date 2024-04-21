from configparser import ConfigParser
import os

config_dir = os.path.dirname(os.path.abspath(__file__))

config = ConfigParser()
config.read(os.path.join(config_dir,'config.ini'))

KEY = config.get('Config','Key')
API = 'https://api.seniverse.com/v3/weather/now.json'
UNIT = 'c'
LANGUAGE = 'zh-Hans'
