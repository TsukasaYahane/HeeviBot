from configparser import ConfigParser
import os

config_dir = os.path.dirname(os.path.abspath(__file__))

config = ConfigParser()
config.read(os.path.join(config_dir,'config.ini'),encoding='utf-8')

KEY = config.get('Config','Key')
UNIT = 'm'
LANGUAGE = 'zh-hans'
headers = {
    'Acept-Language' : 'zh-CN,zh;q=0.7',
    'User-Agent' : 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36',
    'X-QW-Api-Key' : KEY
    }
