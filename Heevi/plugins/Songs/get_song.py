import requests

url = 'https://www.hhlqilongzhu.cn/api/dg_wyymusic.php'
headers = {
    'Accept-Language' : 'zh-CN,zh;q=0.7',
    'User-Agent' : 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36',
    }


class Song_Get():
    def ncm_song_search(name:str):
        param = {
            'gm' : name,
            'type' : 'json'
        }
        result = requests.get(url, headers = headers, params = param, timeout = 10)
        Song_return_Json = result.json()
        return Song_return_Json
    
    def ncm_song_get(name:str,n:int):
        param = {
            'gm' : name,
            'type' : 'json',
            'n' : n
        }
        result = requests.get(url, headers = headers, params = param, timeout = 10)
        Song_return_Json = result.json()
        Song_Link = Song_return_Json['music_url']
        return Song_Link


