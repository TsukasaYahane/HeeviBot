import requests
from io import BytesIO
import base64

def get_videoinfo(BV) :
    url = 'https://api.bilibili.com/x/web-interface/view'
    headers = {
    'Acept-Language' : 'zh-CN,zh;q=0.7',
    'User-Agent' : 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36',
    }
    data = requests.get(url,params={
    'bvid' : BV
    },headers=headers,timeout = 10)
    video_data = data.json()
    return video_data
