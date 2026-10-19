from http import HTTPStatus
from dashscope.audio.asr import Transcription
import json

# 若没有将API Key配置到环境变量中，需将下面这行代码注释放开，并将apiKey替换为自己的API Key
# import dashscope
# dashscope.api_key = "apiKey"

task_response = Transcription.async_call(
    model='paraformer-v2',
    file_urls=['https://dashscope.oss-cn-beijing.aliyuncs.com/samples/audio/paraformer/hello_world_female2.wav',
               'https://dashscope.oss-cn-beijing.aliyuncs.com/samples/audio/paraformer/hello_world_male2.wav']
    # language_hints=['zh', 'en']  # “language_hints”只支持paraformer-v2模型
)

transcribe_response = Transcription.wait(task=task_response.output.task_id)
if transcribe_response.status_code == HTTPStatus.OK:
    print(json.dumps(transcribe_response.output, indent=4, ensure_ascii=False))
    print('transcription done!')